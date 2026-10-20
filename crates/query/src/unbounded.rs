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
//! - its depth-0 WHERE has an empty condition (`DELETE FROM t WHERE`, a dangling
//!   `WHERE` before `ORDER BY` / `LIMIT`): counted as no WHERE;
//! - its depth-0 WHERE is trivially true (see below).
//!
//! A `WHERE` (and `SET`, `ORDER`, `OR`, …) next to a `.` is part of a qualified name
//! (`t.where`, `shop.where`, `where.u`), never the keyword.
//!
//! Trivially true, read inside any wrapping parens:
//!
//! - a depth-0 `OR` (MySQL also `||`: the server's `PIPES_AS_CONCAT` is not known
//!   here, so the strict reading is taken) with one trivially-true operand;
//! - a depth-0 `AND` (MySQL also `&&`) whose operands are all trivially true
//!   (`1=1 AND 1=1`);
//! - `TRUE`, a non-zero integer with an optional sign (`WHERE 1`, `-1`, `+2`);
//! - MySQL: a quoted non-zero integer string (`'1'`, `'-3'`); PG: a quoted boolean
//!   true string (`'t'`, `'true'`, `'y'`, `'yes'`, `'on'`, `'1'`);
//! - `NOT` (MySQL also `!`) of `FALSE` or a zero integer (`NOT 0`, `NOT FALSE`);
//! - `x = x` (MySQL also `x <=> x`) with the same (signed) integer, the same quoted
//!   string or the same bare / quoted / qualified identifier on both sides (`1=1`,
//!   `'a'='a'`, `id = id`, `` `a` = `a` ``; `NULL = NULL` is not true);
//! - `x <> y` / `x != y` with two different integers (`1 <> 0`).
//!
//! Nothing else is judged.
//!
//! Where the statement runs: after a PG/MySQL `WITH …` (the main statement, and every
//! data-modifying CTE body), after `EXPLAIN ANALYZE` (which runs it), after MariaDB
//! `SET STATEMENT … FOR`, and the body after `AS` of a PG `PREPARE p [(types)] AS …`.
//! A WHERE inside a paren group (a subquery in `SET`, `USING`, a join) is not the
//! statement's WHERE.
//!
//! Not judged (out of scope): `MERGE` (its `WHEN [NOT] MATCHED … DELETE / UPDATE`
//! arms may change every row, e.g. `ON true`, but are never flagged), MySQL `PREPARE … FROM '…'` (the text is a
//! string) and `EXECUTE` of a prepared statement.
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
    /// No WHERE at depth 0 (or one with an empty condition).
    NoWhere,
    /// A trivially-true WHERE (`WHERE 1`, `1=1`, `true`, `'a'='a'`, `id = id`).
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

/// The keyword `kw` at `i`: the word, not part of a qualified name (`t.where`,
/// `where.u`).
fn is_kw(toks: &[Token], i: usize, kw: &str) -> bool {
    is_word(toks, i, kw) && !(i > 0 && is_punct(toks, i - 1, b'.')) && !is_punct(toks, i + 1, b'.')
}

/// The two-byte operator `op` (e.g. `||`) at `i`: two adjacent punct tokens.
fn is_op2(toks: &[Token], i: usize, op: &[u8; 2]) -> bool {
    is_punct(toks, i, op[0]) && is_punct(toks, i + 1, op[1]) && toks[i].end == toks[i + 1].start
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
        "UPDATE" => update(src, toks, dialect, out),
        "DELETE" => delete(src, toks, dialect, out),
        // PG `PREPARE p [(types)] AS <statement>`: judge the body.
        "PREPARE" if !dialect.is_mysql() => {
            if let Some(at) = depth0(toks).into_iter().find(|&k| is_kw(toks, k, "AS")) {
                scan(src, &toks[at + 1..], dialect, depth + 1, out);
            }
        }
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
fn where_status(src: &str, toks: &[Token], dialect: Dialect) -> Option<UnboundedReason> {
    let idx = depth0(toks);
    let Some(pos) = idx.iter().position(|&k| is_kw(toks, k, "WHERE")) else {
        return Some(UnboundedReason::NoWhere);
    };
    let from = idx[pos] + 1;
    let to = idx[pos + 1..]
        .iter()
        .copied()
        .find(|&k| {
            ["ORDER", "LIMIT", "RETURNING"]
                .iter()
                .any(|kw| is_kw(toks, k, kw))
        })
        .unwrap_or_else(|| idx.last().map_or(toks.len(), |&l| close_end(toks, l)));
    let cond = &toks[from..to.max(from)];
    if cond.is_empty() {
        // A dangling WHERE bounds nothing.
        return Some(UnboundedReason::NoWhere);
    }
    always_true(src, cond, dialect).then_some(UnboundedReason::AlwaysTrue)
}

/// The index past the depth-0 token at `k` (past its group for an `Open`).
fn close_end(toks: &[Token], k: usize) -> usize {
    if toks[k].kind == TokKind::Open {
        (close_of(toks, k) + 1).min(toks.len())
    } else {
        k + 1
    }
}

/// `cond` without its wrapping parens.
fn unwrap_parens<'a, 'b>(mut cond: &'b [Token<'a>]) -> &'b [Token<'a>] {
    while cond.first().is_some_and(|t| t.kind == TokKind::Open)
        && close_of(cond, 0) == cond.len() - 1
    {
        cond = &cond[1..cond.len() - 1];
    }
    cond
}

/// The operands of `cond` around its depth-0 operators (`op(k)`: the token count of
/// the operator at `k`, 0 when none is there); `None` when it has no operator.
fn split_on<'a, 'b>(
    cond: &'b [Token<'a>],
    op: impl Fn(usize) -> usize,
) -> Option<Vec<&'b [Token<'a>]>> {
    let mut parts = Vec::new();
    let mut from = 0;
    for k in depth0(cond) {
        if k < from {
            continue;
        }
        let n = op(k);
        if n > 0 {
            parts.push(&cond[from..k]);
            from = k + n;
        }
    }
    if parts.is_empty() {
        return None;
    }
    parts.push(&cond[from.min(cond.len())..]);
    Some(parts)
}

/// Whether the condition `cond` (tokens of `src`) is trivially true (see the module
/// docs).
fn always_true(src: &str, cond: &[Token], dialect: Dialect) -> bool {
    let cond = unwrap_parens(cond);
    if cond.is_empty() {
        return false;
    }
    let my = dialect.is_mysql();
    // OR (lowest precedence): any trivially-true operand makes it true.
    let or = |k: usize| {
        if is_kw(cond, k, "OR") {
            1
        } else if my && is_op2(cond, k, b"||") {
            2
        } else {
            0
        }
    };
    if let Some(parts) = split_on(cond, or) {
        return parts.iter().any(|p| always_true(src, p, dialect));
    }
    // AND: every operand must be trivially true.
    let and = |k: usize| {
        if is_kw(cond, k, "AND") {
            1
        } else if my && is_op2(cond, k, b"&&") {
            2
        } else {
            0
        }
    };
    if let Some(parts) = split_on(cond, and) {
        return parts.iter().all(|p| always_true(src, p, dialect));
    }
    if is_kw(cond, 0, "NOT") || (my && is_punct(cond, 0, b'!') && !is_op2(cond, 0, b"!=")) {
        return always_false(src, &cond[1..]);
    }
    // A comparison: the first depth-0 run of adjacent `<`, `=`, `>`, `!`.
    let is_cmp = |k: usize| {
        cond.get(k)
            .is_some_and(|t| matches!(t.kind, TokKind::Punct(b'<' | b'=' | b'>' | b'!')))
    };
    if let Some(at) = depth0(cond).into_iter().find(|&k| is_cmp(k)) {
        let mut end = at + 1;
        while is_cmp(end) && cond[end - 1].end == cond[end].start {
            end += 1;
        }
        let op = &src[cond[at].start..cond[end - 1].end];
        let (l, r) = (value(src, &cond[..at]), value(src, &cond[end..]));
        return match op {
            "=" => same_value(l, r, false),
            "<=>" if my => same_value(l, r, true),
            "<>" | "!=" => {
                matches!((l, r), (Some(a @ Val::Int(..)), Some(b @ Val::Int(..))) if a != b)
            }
            _ => false,
        };
    }
    value(src, cond).is_some_and(|v| truthy(v, dialect))
}

/// Whether `cond` is `FALSE` or a zero integer (inside any wrapping parens).
fn always_false(src: &str, cond: &[Token]) -> bool {
    match value(src, cond) {
        Some(Val::Int(_, digits)) => digits.is_empty(),
        Some(Val::Text(t)) => t.eq_ignore_ascii_case("FALSE"),
        None => false,
    }
}

/// A plain operand of a condition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Val<'s> {
    /// An integer with an optional sign: (negative, digits without leading zeros);
    /// zero is `(false, "")`.
    Int(bool, &'s str),
    /// A (qualified) name, a keyword, a quoted string or identifier, as written.
    Text(&'s str),
}

/// `side` (tokens of `src`) as a [`Val`], `None` when it is anything else.
fn value<'s>(src: &'s str, side: &[Token]) -> Option<Val<'s>> {
    let side = unwrap_parens(side);
    let (neg, rest) = match side.first()?.kind {
        TokKind::Punct(b'-') => (true, &side[1..]),
        TokKind::Punct(b'+') => (false, &side[1..]),
        _ => (false, side),
    };
    if let [t] = rest {
        if let TokKind::Word(w) = t.kind {
            if !w.is_empty() && w.bytes().all(|c| c.is_ascii_digit()) {
                let digits = src[t.start..t.end].trim_start_matches('0');
                return Some(Val::Int(neg && !digits.is_empty(), digits));
            }
        }
    }
    if rest.len() != side.len() {
        // A sign before something else than an integer.
        return None;
    }
    let (text, _) = name_at(src, side, 0)?;
    let whole = &src[side[0].start..side[side.len() - 1].end];
    (text == whole).then_some(Val::Text(whole))
}

/// `l = r` (or `l <=> r` when `null_safe`) holds on every row.
fn same_value(l: Option<Val>, r: Option<Val>, null_safe: bool) -> bool {
    match (l, r) {
        (Some(Val::Int(a, x)), Some(Val::Int(b, y))) => a == b && x == y,
        (Some(Val::Text(x)), Some(Val::Text(y))) => {
            x == y && (null_safe || !x.eq_ignore_ascii_case("NULL"))
        }
        _ => false,
    }
}

/// Whether the single operand `v` is true as a whole condition.
fn truthy(v: Val, dialect: Dialect) -> bool {
    let text = match v {
        Val::Int(_, digits) => return !digits.is_empty(),
        Val::Text(t) => t,
    };
    if text.eq_ignore_ascii_case("TRUE") {
        return true;
    }
    let quoted = |q: char| text.len() >= 2 && text.starts_with(q) && text.ends_with(q);
    match dialect {
        Dialect::MySql(m) => {
            // A string is read as a number: `'1'` is true.
            if !(quoted('\'') || (!m.ansi_quotes && quoted('"'))) {
                return false;
            }
            let body = text[1..text.len() - 1].trim();
            let digits = body.strip_prefix(['-', '+']).unwrap_or(body);
            !digits.is_empty()
                && digits.bytes().all(|c| c.is_ascii_digit())
                && digits.bytes().any(|c| c != b'0')
        }
        Dialect::Postgres => {
            // An unknown-typed literal read as a boolean.
            quoted('\'')
                && ["t", "true", "y", "yes", "on", "1"]
                    .iter()
                    .any(|b| text[1..text.len() - 1].trim().eq_ignore_ascii_case(b))
        }
    }
}

fn update(src: &str, toks: &[Token], dialect: Dialect, out: &mut Vec<UnboundedWrite>) {
    let i = skip_words(toks, 1, &["LOW_PRIORITY", "IGNORE", "ONLY"]);
    let idx = depth0(toks);
    let set = idx
        .iter()
        .copied()
        .find(|&k| k >= i && is_kw(toks, k, "SET"))
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
    if let Some(reason) = where_status(src, &toks[set.min(toks.len())..], dialect) {
        out.push(UnboundedWrite {
            op: WriteOp::Update,
            reason,
            table: names.join(", "),
        });
    }
}

fn delete(src: &str, toks: &[Token], dialect: Dialect, out: &mut Vec<UnboundedWrite>) {
    let i = skip_words(toks, 1, &["LOW_PRIORITY", "QUICK", "IGNORE"]);
    let names = if is_word(toks, i, "FROM") {
        // `DELETE FROM t …`, `DELETE FROM t1, t2 USING …`
        name_list(src, toks, i + 1).0
    } else {
        // MySQL `DELETE t1, t2 FROM …`
        name_list(src, toks, i).0
    };
    if let Some(reason) = where_status(src, &toks[i.min(toks.len())..], dialect) {
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
                "DELETE FROM t WHERE a = b",
                "DELETE FROM t WHERE a = t.a",
                "DELETE FROM t WHERE NULL = NULL",
                "DELETE FROM t WHERE 1=1 AND id = 3",
                "DELETE FROM t WHERE 1=1 AND 1=0",
                "DELETE FROM t WHERE (id = 3 OR id = 4)",
                "UPDATE t SET a = 1 WHERE id = 1 ORDER BY id LIMIT 1",
                "DELETE FROM t WHERE 'a' = 'b'",
                "DELETE FROM t WHERE 'a' <> 'b'",
                "DELETE FROM t WHERE 1 <> 1",
                "DELETE FROM t WHERE 1 != 01",
                "DELETE FROM t WHERE 1 = -1",
                "DELETE FROM t WHERE -0",
                "DELETE FROM t WHERE NOT 1",
                "DELETE FROM t WHERE NOT TRUE",
                "DELETE FROM t WHERE NOT 0 = 0",
                "DELETE FROM t WHERE NOT a",
                "DELETE FROM t WHERE '0'",
                "DELETE FROM t WHERE 'abc'",
                "DELETE FROM t WHERE a BETWEEN 1 AND 2",
                "DELETE FROM t WHERE a > 1",
                "DELETE FROM t WHERE -a",
            ] {
                assert_eq!(flags(s, d), [], "{s} ({d:?})");
            }
        }
        assert_eq!(flags("DELETE FROM t WHERE CURRENT OF c", PG), []);
        // `||` is concatenation in PG, `&&` an array operator.
        assert_eq!(flags("DELETE FROM t WHERE a = 'x' || 1", PG), []);
        assert_eq!(flags("DELETE FROM t WHERE a || 1", PG), []);
        assert_eq!(flags("DELETE FROM t WHERE a && 1", PG), []);
        // `<=>` is MySQL only.
        assert_eq!(flags("DELETE FROM t WHERE a <=> a", PG), []);
        // Under ANSI_QUOTES `"1"` is an identifier.
        let ansi = Dialect::MySql(dnt_core::sql::MySqlMode {
            ansi_quotes: true,
            no_backslash_escapes: false,
        });
        assert_eq!(flags(r#"DELETE FROM t WHERE "1""#, ansi), []);
        assert_eq!(flags(r#"DELETE FROM t WHERE "1""#, PG), []);
    }

    #[test]
    fn a_where_in_a_qualified_name_is_not_the_keyword() {
        for d in BOTH {
            assert_eq!(
                flags("UPDATE t SET a = t.where", d),
                one(Update, NoWhere, "t"),
                "{d:?}"
            );
            assert_eq!(
                flags("DELETE FROM shop.where", d),
                one(Delete, NoWhere, "shop.where")
            );
            assert_eq!(
                flags("UPDATE t SET a = where.b", d),
                one(Update, NoWhere, "t")
            );
            assert_eq!(flags("UPDATE t SET a = t.where WHERE id = 1", d), []);
            assert_eq!(flags("DELETE FROM shop.where WHERE id = 1", d), []);
        }
        assert_eq!(
            flags("DELETE FROM t ORDER BY t.where LIMIT 5", MY),
            one(Delete, NoWhere, "t")
        );
        assert_eq!(
            flags("UPDATE t SET a = 1 FROM where.u", PG),
            one(Update, NoWhere, "t")
        );
        assert_eq!(
            flags("UPDATE x.set SET a = 1", MY),
            one(Update, NoWhere, "x.set")
        );
        // `t.order` / `t.limit` do not end the condition.
        assert_eq!(
            flags("DELETE FROM t WHERE 1 = 1 AND t.order = t.order", MY),
            one(Delete, AlwaysTrue, "t")
        );
        assert_eq!(flags("DELETE FROM t WHERE t.limit = 1", MY), []);
    }

    #[test]
    fn a_dangling_where_is_no_where() {
        for d in BOTH {
            for s in [
                "DELETE FROM t WHERE",
                "DELETE FROM t WHERE;",
                "UPDATE t SET a = 1 WHERE",
                "DELETE FROM t WHERE -- id = 1",
            ] {
                assert_eq!(flags(s, d).len(), 1, "{s} ({d:?})");
                assert_eq!(flags(s, d)[0].1, NoWhere, "{s} ({d:?})");
            }
        }
        assert_eq!(
            flags("DELETE FROM t WHERE ORDER BY id LIMIT 1", MY),
            one(Delete, NoWhere, "t")
        );
        assert_eq!(
            flags("DELETE FROM t WHERE RETURNING *", PG),
            one(Delete, NoWhere, "t")
        );
    }

    #[test]
    fn more_trivially_true_forms() {
        for d in BOTH {
            for s in [
                "DELETE FROM t WHERE 1=1 AND 1=1",
                "DELETE FROM t WHERE (1 = 1) AND true AND 2",
                "DELETE FROM t WHERE id = 3 OR (1=1 AND 1)",
                "DELETE FROM t WHERE NOT 0",
                "DELETE FROM t WHERE NOT FALSE",
                "DELETE FROM t WHERE NOT (0)",
                "DELETE FROM t WHERE -1",
                "DELETE FROM t WHERE +1",
                "DELETE FROM t WHERE (-5)",
                "DELETE FROM t WHERE 1 <> 0",
                "DELETE FROM t WHERE 1 != 2",
                "DELETE FROM t WHERE -1 <> 1",
                "DELETE FROM t WHERE -1 = -1",
                "DELETE FROM t WHERE 01 = 1",
                "DELETE FROM t WHERE a = a",
                "DELETE FROM t WHERE t.a = t.a",
                "DELETE FROM t WHERE id = 1 OR a = a",
            ] {
                assert_eq!(flags(s, d), one(Delete, AlwaysTrue, "t"), "{s} ({d:?})");
            }
        }
        for s in [
            "DELETE FROM t WHERE id = 5 || 1",
            "DELETE FROM t WHERE id = 5||1=1",
            "DELETE FROM t WHERE 1 && 1",
            "DELETE FROM t WHERE !0",
            "DELETE FROM t WHERE ! FALSE",
            "DELETE FROM t WHERE '1'",
            "DELETE FROM t WHERE '-2'",
            "DELETE FROM t WHERE ' 10 '",
            "DELETE FROM t WHERE \"1\"",
            "DELETE FROM t WHERE `a` = `a`",
            "DELETE FROM t WHERE a <=> a",
            "DELETE FROM t WHERE NULL <=> NULL",
        ] {
            assert_eq!(flags(s, MY), one(Delete, AlwaysTrue, "t"), "{s}");
        }
        for s in [
            "DELETE FROM t WHERE 't'",
            "DELETE FROM t WHERE 'TRUE'",
            "DELETE FROM t WHERE 'yes'",
            "DELETE FROM t WHERE 'on'",
            "DELETE FROM t WHERE '1'",
            r#"DELETE FROM t WHERE "a" = "a""#,
        ] {
            assert_eq!(flags(s, PG), one(Delete, AlwaysTrue, "t"), "{s}");
        }
        assert_eq!(flags("DELETE FROM t WHERE 'f'", PG), []);
        assert_eq!(flags("DELETE FROM t WHERE 'yes'", MY), []);
    }

    #[test]
    fn pg_prepare_body_is_judged() {
        assert_eq!(
            flags("PREPARE p AS DELETE FROM t", PG),
            one(Delete, NoWhere, "t")
        );
        assert_eq!(
            flags("PREPARE p (int) AS UPDATE t SET a = $1", PG),
            one(Update, NoWhere, "t")
        );
        assert_eq!(
            flags("prepare p(int, text) as delete from t where 1 = 1", PG),
            one(Delete, AlwaysTrue, "t")
        );
        assert_eq!(
            flags("PREPARE p (int) AS DELETE FROM t WHERE id = $1", PG),
            []
        );
        assert_eq!(flags("PREPARE p AS SELECT * FROM t", PG), []);
        // MySQL's text is a string.
        assert_eq!(flags("PREPARE p FROM 'DELETE FROM t'", MY), []);
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
