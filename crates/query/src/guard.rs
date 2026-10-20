//! Production / read-only guard (spec §5.1): the verdict of every planned
//! statement. The backend applies it at plan time and again at `query_run`.
//!
//! | Kind                         | Production (not read-only) | Read-only (any tag) | Other |
//! |------------------------------|----------------------------|---------------------|-------|
//! | `Read`, `Transaction(_)`     | Allow                      | Allow               | Allow |
//! | `Write`, `Ddl`               | Confirm                    | Block               | Allow |
//! | whitelisted `Other`          | Allow                      | Allow               | Allow |
//! | other `Other`                | Confirm                    | Block               | Allow |
//! | plain Explain (not `Ddl`)    | Allow                      | Allow               | Allow |
//! | Explain Analyze (`Read`)     | Confirm                    | Allow               | Allow |
//!
//! On a read-only connection, a statement that lifts the session's read-only
//! mode ([`lifts_read_only`]) is Block whatever its kind.
//!
//! A planned statement that holds several statements for the server (a MySQL
//! `DELIMITER` left on, see [`resplits`]) gets the strictest verdict over all of
//! them, and plain Explain does not cover it.
//!
//! Keyword positions are read from [`tokenize`], which keeps quoted tokens in
//! place, so `SET \`a\` GLOBAL …` never passes for a session `SET`.

use dnt_core::config::EnvTag;
use dnt_core::sql::{classify, readings, split, tokenize, Dialect, StatementKind, TokKind, Token};

use crate::plan::{ScriptPlan, Verdict};

/// Sets the verdict of every statement in `plan`.
///
/// - `analyze` (Explain Analyze; the planner guarantees `Read`): Production →
///   Confirm, read-only → Allow, otherwise Allow. A statement that is not `Read`
///   still gets its ordinary verdict when that is stricter.
/// - Otherwise, when `plan.explain` is set (plain Explain), every statement but a
///   `Ddl` is Allow; a `Ddl` gets its ordinary verdict.
/// - Otherwise, [`verdict`].
///
/// The inputs are kept in `plan.guard`, so `run` can judge the statements again
/// when the session's `sql_mode` changes how they are lexed.
pub fn guard(plan: &mut ScriptPlan, env: Option<EnvTag>, read_only: bool, analyze: bool) {
    let ctx = GuardCtx {
        env,
        read_only,
        analyze,
    };
    let (dialect, explain) = (plan.dialect, plan.explain);
    for s in &mut plan.stmts {
        s.verdict = stmt_verdict(&s.text, s.kind, dialect, explain, ctx);
    }
    plan.guard = Some(ctx);
}

/// What [`guard`] judged a plan with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuardCtx {
    pub env: Option<EnvTag>,
    pub read_only: bool,
    pub analyze: bool,
}

/// The verdict [`guard`] gives one statement of a plan (`explain`: the plan's flag).
pub(crate) fn stmt_verdict(
    text: &str,
    kind: StatementKind,
    dialect: Dialect,
    explain: bool,
    ctx: GuardCtx,
) -> Verdict {
    let GuardCtx {
        env,
        read_only,
        analyze,
    } = ctx;
    if analyze {
        let analyze_verdict = if !read_only && env == Some(EnvTag::Production) {
            Verdict::Confirm
        } else {
            Verdict::Allow
        };
        strictest(
            analyze_verdict,
            verdict(text, kind, dialect, env, read_only),
        )
    } else if explain && kind != StatementKind::Ddl && !resplits(text, dialect) {
        // `explain_sql` refuses a text that re-splits; judged like a run should
        // one reach here.
        Verdict::Allow
    } else {
        verdict(text, kind, dialect, env, read_only)
    }
}

/// The §5.1 verdict of one statement: the strictest over its [`readings`], each
/// classified with [`classify`]. `kind` (the plan's kind) can only make it
/// stricter. Read-only takes precedence over the Production tag; any other tag
/// (or none) allows everything.
pub fn verdict(
    stmt: &str,
    kind: StatementKind,
    dialect: Dialect,
    env: Option<EnvTag>,
    read_only: bool,
) -> Verdict {
    if !read_only && env != Some(EnvTag::Production) {
        return Verdict::Allow;
    }
    let unsafe_verdict = if read_only {
        Verdict::Block
    } else {
        Verdict::Confirm
    };
    let mut v = match kind {
        StatementKind::Write | StatementKind::Ddl => unsafe_verdict,
        _ => Verdict::Allow,
    };
    let judge = |r: &str, toks: &[Token]| {
        let mut rv = if read_only && lifts_toks(r, toks, dialect) {
            Verdict::Block
        } else {
            match classify(r, dialect) {
                StatementKind::Read | StatementKind::Transaction(_) => Verdict::Allow,
                StatementKind::Write | StatementKind::Ddl => unsafe_verdict,
                StatementKind::Other if whitelisted_toks(toks, dialect) => Verdict::Allow,
                StatementKind::Other => unsafe_verdict,
            }
        };
        // `EXPLAIN ANALYZE …` typed in the editor runs its statement, like the
        // `analyze` flag of `guard`: Confirm on Production, Allow on read-only.
        if !read_only && explain_analyze(toks, dialect) {
            rv = strictest(rv, Verdict::Confirm);
        }
        rv
    };
    let mut any = false;
    for r in readings(stmt, dialect) {
        let toks = tokenize(&r, dialect);
        // A reading with no tokens sends nothing (an executable comment the
        // server ignores).
        if toks.is_empty() {
            continue;
        }
        any = true;
        v = strictest(v, judge(&r, &toks));
        // A reading that holds several statements (see `resplits`): the server
        // runs each of them, so each is judged too.
        for piece in pieces(&r, &toks, dialect).into_iter().flatten() {
            let ptoks = tokenize(&piece, dialect);
            if !ptoks.is_empty() {
                v = strictest(v, judge(&piece, &ptoks));
            }
        }
    }
    if !any && kind == StatementKind::Other {
        v = strictest(v, unsafe_verdict);
    }
    v
}

/// Whether `stmt` is an `Other` statement on the §5.1 whitelist, in every one of
/// its non-empty [`readings`]:
///
/// - both dialects: `SAVEPOINT`, `RELEASE [SAVEPOINT]`, `ROLLBACK [WORK|TRANSACTION]
///   TO`, `PREPARE` (not PG `PREPARE TRANSACTION`), `DEALLOCATE`;
/// - MySQL: `USE`, and `SET` whose every assignment is session-scoped (none
///   starts with `GLOBAL`, `PERSIST`, `PERSIST_ONLY`, `@@global.`, `@@persist.`,
///   `@@persist_only.`, nor is `SET DEFAULT ROLE`, `SET RESOURCE GROUP` or MariaDB
///   `SET STATEMENT … FOR`);
/// - PostgreSQL: `SET`, `RESET`, `DISCARD`, `LISTEN`, `UNLISTEN`, `DECLARE`,
///   `FETCH`, `MOVE`, `CLOSE`.
pub fn other_whitelisted(stmt: &str, dialect: Dialect) -> bool {
    let mut any = false;
    for r in readings(stmt, dialect) {
        let toks = tokenize(&r, dialect);
        if toks.is_empty() {
            continue;
        }
        if !whitelisted_toks(&toks, dialect) {
            return false;
        }
        any = true;
    }
    any
}

/// Whether some reading of `stmt` lifts the session's read-only mode:
///
/// - `SET … READ WRITE` (`SET [SESSION|GLOBAL] TRANSACTION … READ WRITE`, PG
///   `SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE`), and also
///   `BEGIN|START TRANSACTION … READ WRITE`, which lifts it for one transaction;
/// - a `SET` assigning `transaction_read_only`, `tx_read_only` or
///   `default_transaction_read_only` any value other than `1`/`on`/`true`
///   (so `0`/`off`/`false`, and also `DEFAULT` or an expression);
/// - PG: any `RESET`, and `DISCARD ALL`;
/// - PG: a call `set_config(name, …)` anywhere in the statement (bare,
///   `pg_catalog.set_config` or quoted `"set_config"`) whose first argument is
///   the literal `'default_transaction_read_only'` or `'transaction_read_only'`,
///   or is not a single string literal at all (a column, `||`, a `$1`).
///   Functions that call `set_config` internally are invisible to this check;
///   the server-side `READ ONLY` session (P1 §4.5) is the first layer there.
///
/// Read-only names are matched case-insensitively, quoted or not; a `SET`
/// assignment whose name is neither a word nor a quoted name also counts.
pub fn lifts_read_only(stmt: &str, dialect: Dialect) -> bool {
    readings(stmt, dialect)
        .iter()
        .any(|r| lifts_toks(r, &tokenize(r, dialect), dialect))
}

/// Whether `stmt` (one planned statement) holds more than one statement for the
/// server: some reading of it, split again with the default `;` delimiter, gives
/// several pieces. Under a MySQL `DELIMITER` a planned statement may hold depth-0
/// `;` (a forgotten `DELIMITER ;`), and with multi-statements on the server runs
/// every piece. Compound statements ([`compound_body`]) never re-split: their `;`
/// belong to the body. A PostgreSQL statement never re-splits (PG has no
/// `DELIMITER`, so `split` already cut at every depth-0 `;`).
pub fn resplits(stmt: &str, dialect: Dialect) -> bool {
    readings(stmt, dialect).iter().any(|r| {
        let toks = tokenize(r, dialect);
        pieces(r, &toks, dialect).is_some()
    })
}

/// The kinds of the pieces of `stmt` ([`resplits`]), reading by reading, each in
/// order; empty when it does not re-split.
pub(crate) fn piece_kinds(stmt: &str, dialect: Dialect) -> Vec<StatementKind> {
    readings(stmt, dialect)
        .iter()
        .filter_map(|r| pieces(r, &tokenize(r, dialect), dialect))
        .flatten()
        .map(|p| classify(&p, dialect))
        .collect()
}

/// The statements of reading `r` (tokens `toks`) split again with the default `;`
/// delimiter, when there are several (see [`resplits`]).
fn pieces(r: &str, toks: &[Token], dialect: Dialect) -> Option<Vec<String>> {
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

fn rank(v: Verdict) -> u8 {
    match v {
        Verdict::Allow => 0,
        Verdict::Confirm => 1,
        Verdict::Block => 2,
    }
}

fn strictest(a: Verdict, b: Verdict) -> Verdict {
    if rank(b) > rank(a) {
        b
    } else {
        a
    }
}

/// The word at token `i`, if that token is a word.
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

/// Whether any word pair `READ WRITE` appears in `toks`.
fn has_read_write(toks: &[Token]) -> bool {
    (1..toks.len()).any(|i| is_word(toks, i - 1, "READ") && is_word(toks, i, "WRITE"))
}

/// The depth-0, comma-separated assignments of a `SET` (tokens after `SET`).
/// Empty groups are dropped.
fn assignments<'t, 'a>(toks: &'t [Token<'a>]) -> Vec<&'t [Token<'a>]> {
    let mut out = Vec::new();
    let (mut depth, mut from) = (0usize, 0);
    for (i, t) in toks.iter().enumerate() {
        match t.kind {
            TokKind::Open => depth += 1,
            TokKind::Close => depth = depth.saturating_sub(1),
            TokKind::Punct(b',') if depth == 0 => {
                out.push(&toks[from..i]);
                from = i + 1;
            }
            _ => {}
        }
    }
    out.push(&toks[from..]);
    out.retain(|a| !a.is_empty());
    out
}

fn whitelisted_toks(toks: &[Token], dialect: Dialect) -> bool {
    let mysql = dialect.is_mysql();
    let Some(first) = word(toks, 0) else {
        return false;
    };
    match first.to_ascii_uppercase().as_str() {
        "SAVEPOINT" | "RELEASE" | "DEALLOCATE" => true,
        // PG `PREPARE TRANSACTION` is two-phase commit, not a prepared statement.
        "PREPARE" => !is_word(toks, 1, "TRANSACTION"),
        "ROLLBACK" => {
            let i = if is_word(toks, 1, "WORK") || is_word(toks, 1, "TRANSACTION") {
                2
            } else {
                1
            };
            is_word(toks, i, "TO")
        }
        "USE" => mysql,
        "SET" if mysql => mysql_session_set(&toks[1..]),
        "SET" | "RESET" | "DISCARD" | "LISTEN" | "UNLISTEN" | "DECLARE" | "FETCH" | "MOVE"
        | "CLOSE" => !mysql,
        _ => false,
    }
}

/// MySQL `SET` (tokens after `SET`) whose every assignment is session-scoped.
fn mysql_session_set(toks: &[Token]) -> bool {
    let groups = assignments(toks);
    !groups.is_empty()
        && groups.iter().all(|a| {
            // A quoted or punctuated target is never proven session-scoped.
            let Some(target) = word(a, 0) else {
                return false;
            };
            let upper = target.to_ascii_uppercase();
            let banned = [
                "GLOBAL",
                "PERSIST",
                "PERSIST_ONLY",
                "DEFAULT",
                "RESOURCE",
                "STATEMENT",
                "@@",
                "@@GLOBAL",
                "@@PERSIST",
                "@@PERSIST_ONLY",
            ];
            !banned.contains(&upper.as_str())
        })
}

/// The read-only switches a `SET` or `set_config` may turn off.
const READ_ONLY_VARS: [&str; 3] = [
    "transaction_read_only",
    "tx_read_only",
    "default_transaction_read_only",
];

/// The body of a quoted token, or `None` when it is unterminated or not quoted:
/// `'…'`, `"…"`, `` `…` `` (a doubled quote stands for one), `E'…'` (backslashes
/// dropped) and `$tag$…$tag$`.
fn unquote(q: &str) -> Option<String> {
    let b = q.as_bytes();
    match *b.first()? {
        c @ (b'\'' | b'"' | b'`') => (q.len() >= 2 && b[q.len() - 1] == c).then(|| {
            let c = char::from(c);
            q[1..q.len() - 1].replace(&format!("{c}{c}"), &c.to_string())
        }),
        b'E' | b'e' => Some(unquote(&q[1..])?.replace('\\', "")),
        b'$' => {
            let tag_len = q[1..].find('$')? + 2;
            let tag = &q[..tag_len];
            (q.len() >= 2 * tag_len && q.ends_with(tag))
                .then(|| q[tag_len..q.len() - tag_len].to_string())
        }
        _ => None,
    }
}

/// The name at token `i`, lowercased: a word, or a quoted name unquoted.
fn name_at(src: &str, toks: &[Token], i: usize) -> Option<String> {
    let t = toks.get(i)?;
    match t.kind {
        TokKind::Word(w) => Some(w.to_ascii_lowercase()),
        TokKind::Quoted => unquote(&src[t.start..t.end]).map(|n| n.to_ascii_lowercase()),
        _ => None,
    }
}

/// `EXPLAIN|DESC|DESCRIBE ANALYZE …` (MySQL), `EXPLAIN ANALYZE|ANALYSE …` and
/// `EXPLAIN (… ANALYZE [value] …) …` (PG) with a value other than
/// `false`/`off`/`0`: the same option rules as `classify`.
fn explain_analyze(toks: &[Token], dialect: Dialect) -> bool {
    let explain = is_word(toks, 0, "EXPLAIN")
        || (dialect.is_mysql() && (is_word(toks, 0, "DESC") || is_word(toks, 0, "DESCRIBE")));
    if !explain {
        return false;
    }
    let is_analyze = |opt: &[Token], i| is_word(opt, i, "ANALYZE") || is_word(opt, i, "ANALYSE");
    if is_analyze(toks, 1) {
        return true;
    }
    if toks.get(1).is_some_and(|t| t.kind == TokKind::Open) {
        let close = toks
            .iter()
            .position(|t| t.kind == TokKind::Close)
            .unwrap_or(toks.len());
        return toks[2..close]
            .split(|t| t.kind == TokKind::Punct(b','))
            .any(|opt| {
                is_analyze(opt, 0) && !["FALSE", "OFF", "0"].iter().any(|v| is_word(opt, 1, v))
            });
    }
    false
}

/// PG: some `set_config(` call in `toks` whose first argument is a read-only
/// switch or not a single string literal (see [`lifts_read_only`]).
fn calls_set_config_read_only(src: &str, toks: &[Token]) -> bool {
    (0..toks.len()).any(|i| {
        let t = toks[i];
        let is_fn = match t.kind {
            TokKind::Word(w) => w.eq_ignore_ascii_case("set_config"),
            TokKind::Quoted => {
                let q = &src[t.start..t.end];
                q.starts_with('"')
                    && unquote(q).is_some_and(|n| n.eq_ignore_ascii_case("set_config"))
            }
            _ => false,
        };
        is_fn
            && toks.get(i + 1).is_some_and(|t| t.kind == TokKind::Open)
            && first_arg_lifts(src, &toks[i + 2..])
    })
}

/// `rest` follows `set_config(`: the first argument ends at a depth-0 `,` or
/// the closing `)`.
fn first_arg_lifts(src: &str, rest: &[Token]) -> bool {
    let mut depth = 0usize;
    let end = rest
        .iter()
        .position(|t| match t.kind {
            TokKind::Open => {
                depth += 1;
                false
            }
            TokKind::Close if depth == 0 => true,
            TokKind::Close => {
                depth -= 1;
                false
            }
            TokKind::Punct(b',') => depth == 0,
            _ => false,
        })
        .unwrap_or(rest.len());
    match &rest[..end] {
        [t] if t.kind == TokKind::Quoted && !src[t.start..].starts_with('"') => {
            match unquote(&src[t.start..t.end]) {
                Some(n) => ["default_transaction_read_only", "transaction_read_only"]
                    .contains(&n.to_ascii_lowercase().as_str()),
                None => true,
            }
        }
        _ => true,
    }
}

fn lifts_toks(src: &str, toks: &[Token], dialect: Dialect) -> bool {
    let mysql = dialect.is_mysql();
    if !mysql && calls_set_config_read_only(src, toks) {
        return true;
    }
    let Some(first) = word(toks, 0) else {
        return false;
    };
    match first.to_ascii_uppercase().as_str() {
        "RESET" => !mysql,
        "DISCARD" => !mysql && is_word(toks, 1, "ALL"),
        "BEGIN" | "START" => has_read_write(toks),
        "SET" => {
            has_read_write(toks)
                || assignments(&toks[1..])
                    .iter()
                    .any(|a| sets_read_only_off(src, a))
        }
        _ => false,
    }
}

/// One `SET` assignment: `[SESSION|LOCAL|GLOBAL|PERSIST|PERSIST_ONLY] name` or
/// `@@[scope.]name`, then `=`, `:=` or `TO`, then a value. A name is a word or a
/// quoted name (matched case-insensitively); any other name shape counts as
/// lifting. True when `name` is a read-only switch and the value is not plainly
/// on (`1`/`on`/`true`, optionally quoted); an unrecognised shape of such an
/// assignment also counts as lifting.
fn sets_read_only_off(src: &str, a: &[Token]) -> bool {
    const SCOPES: [&str; 5] = ["SESSION", "LOCAL", "GLOBAL", "PERSIST", "PERSIST_ONLY"];
    let mut i = usize::from(SCOPES.iter().any(|s| is_word(a, 0, s)));
    let Some(name) = name_at(src, a, i) else {
        return true;
    };
    let var = match name.strip_prefix("@@") {
        // `@@scope.name`, or `@@` then a quoted name.
        Some(rest) if rest.is_empty() || is_punct(a, i + 1, b'.') => {
            i += if rest.is_empty() { 1 } else { 2 };
            match name_at(src, a, i) {
                Some(n) => n,
                None => return true,
            }
        }
        Some(rest) => rest.to_string(),
        None => name,
    };
    if !READ_ONLY_VARS.contains(&var.as_str()) {
        return false;
    }
    i += 1;
    if is_punct(a, i, b':') {
        i += 1;
    }
    if is_punct(a, i, b'=') || is_word(a, i, "TO") {
        i += 1;
    } else {
        return true;
    }
    if i + 1 != a.len() {
        return true;
    }
    let Some(value) = name_at(src, a, i) else {
        return true;
    };
    !["1", "ON", "TRUE"]
        .iter()
        .any(|on| value.eq_ignore_ascii_case(on))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{plan, Target};
    use dnt_core::sql::{classify, split};
    use Verdict::{Allow as A, Block as B, Confirm as C};

    const MY: Dialect = Dialect::MYSQL;
    const PG: Dialect = Dialect::Postgres;
    const BOTH: &[Dialect] = &[MY, PG];

    /// Verdicts under (Production, read-only, Local), the three columns of §5.1.
    fn columns(stmt: &str, d: Dialect) -> [Verdict; 3] {
        let k = classify(stmt, d);
        [
            verdict(stmt, k, d, Some(EnvTag::Production), false),
            verdict(stmt, k, d, Some(EnvTag::Local), true),
            verdict(stmt, k, d, Some(EnvTag::Local), false),
        ]
    }

    /// The same three columns through `plan` + `guard`.
    fn plan_columns(text: &str, d: Dialect, explain: bool, analyze: bool) -> [Verdict; 3] {
        let cols = [
            (Some(EnvTag::Production), false),
            (Some(EnvTag::Local), true),
            (None, false),
        ];
        cols.map(|(env, ro)| {
            let mut p = plan(text, Target::All, d);
            p.explain = explain;
            guard(&mut p, env, ro, analyze);
            assert_eq!(p.stmts.len(), 1, "{text}");
            p.stmts[0].verdict
        })
    }

    #[test]
    fn guard_matrix() {
        type Row = (&'static [&'static str], &'static [Dialect], [Verdict; 3]);
        let rows: &[Row] = &[
            (
                &["SELECT 1", "BEGIN", "COMMIT", "SET autocommit = 0"],
                BOTH,
                [A, A, A],
            ),
            (
                &[
                    "INSERT INTO t VALUES (1)",
                    "CREATE TABLE t (id int)",
                    "DROP TABLE t",
                    "CALL p()",
                ],
                BOTH,
                [C, B, A],
            ),
            (
                &[
                    "SAVEPOINT s",
                    "RELEASE SAVEPOINT s",
                    "ROLLBACK TO s",
                    "DEALLOCATE PREPARE p",
                ],
                &[MY],
                [A, A, A],
            ),
            (
                &["SAVEPOINT s", "RELEASE s", "ROLLBACK TO s", "DEALLOCATE p"],
                &[PG],
                [A, A, A],
            ),
            (&["PREPARE p FROM 'SELECT 1'"], &[MY], [A, A, A]),
            (&["PREPARE p AS SELECT 1"], &[PG], [A, A, A]),
            (
                &[
                    "USE shop",
                    "SET NAMES utf8mb4",
                    "SET @a = 1",
                    "SET SESSION sql_mode = ''",
                ],
                &[MY],
                [A, A, A],
            ),
            (
                &[
                    "SET GLOBAL max_connections = 10",
                    "SET PERSIST x = 1",
                    "SET PERSIST_ONLY x = 1",
                    "SET @@global.x = 1",
                    "SET @@persist.x = 1",
                ],
                &[MY],
                [C, B, A],
            ),
            (
                &[
                    "SET search_path = x",
                    "DISCARD PLANS",
                    "LISTEN c",
                    "UNLISTEN *",
                    "DECLARE c CURSOR FOR SELECT 1",
                    "FETCH c",
                    "MOVE c",
                    "CLOSE c",
                ],
                &[PG],
                [A, A, A],
            ),
            (
                &["RESET work_mem", "RESET ALL", "DISCARD ALL"],
                &[PG],
                [A, B, A],
            ),
            (
                &[
                    "SET SESSION TRANSACTION READ WRITE",
                    "SET tx_read_only = 0",
                    "SET SESSION transaction_read_only = OFF",
                ],
                &[MY],
                [A, B, A],
            ),
            (
                &[
                    "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE",
                    "SET default_transaction_read_only TO false",
                ],
                &[PG],
                [A, B, A],
            ),
            (
                &[
                    "RESET MASTER",
                    "FLUSH TABLES",
                    "PURGE BINARY LOGS BEFORE '2020-01-01'",
                    "KILL 12",
                    "LOCK TABLES t WRITE",
                    "HANDLER t OPEN",
                    "CHANGE MASTER TO MASTER_HOST='h'",
                    "CHANGE REPLICATION SOURCE TO SOURCE_HOST='h'",
                    "START SLAVE",
                    "STOP REPLICA",
                    "ANALYZE TABLE t",
                ],
                &[MY],
                [C, B, A],
            ),
            (&["NOTIFY c"], &[PG], [C, B, A]),
            (&["/*!40101 SET NAMES utf8 */"], &[MY], [A, A, A]),
            (
                &[
                    "/*!50001 DROP TABLE t */",
                    "/*!40000 ALTER TABLE t DISABLE KEYS */",
                ],
                &[MY],
                [C, B, A],
            ),
        ];
        for (stmts, dialects, want) in rows {
            for &d in *dialects {
                for &s in *stmts {
                    assert_eq!(columns(s, d), *want, "{s} ({d:?})");
                    assert_eq!(plan_columns(s, d, false, false), *want, "plan: {s} ({d:?})");
                }
            }
        }
    }

    #[test]
    fn staging_and_untagged_are_like_local() {
        for env in [Some(EnvTag::Staging), None] {
            for s in ["DROP TABLE t", "KILL 1", "SET GLOBAL x = 1"] {
                assert_eq!(verdict(s, classify(s, MY), MY, env, false), A, "{s}");
            }
        }
    }

    #[test]
    fn read_only_wins_over_production() {
        for s in ["DROP TABLE t", "KILL 1", "SET tx_read_only = 0"] {
            let v = verdict(s, classify(s, MY), MY, Some(EnvTag::Production), true);
            assert_eq!(v, B, "{s}");
        }
        let s = "SET NAMES utf8";
        assert_eq!(
            verdict(s, classify(s, MY), MY, Some(EnvTag::Production), true),
            A
        );
    }

    #[test]
    fn whitelist_is_per_dialect() {
        // MySQL-only and PG-only entries do not cross over.
        for s in [
            "LISTEN c",
            "DISCARD PLANS",
            "FETCH c",
            "DECLARE c CURSOR FOR SELECT 1",
        ] {
            assert_eq!(columns(s, MY), [C, B, A], "{s}");
        }
        assert_eq!(columns("USE shop", PG), [C, B, A]);
        assert_eq!(columns("RESET QUERY CACHE", MY), [C, B, A]);
    }

    #[test]
    fn session_set_rejects_any_non_session_assignment() {
        for s in [
            "SET NAMES utf8, GLOBAL max_connections = 10",
            "SET @a = 1, @@GLOBAL.x = 1",
            "SET sql_mode = '', persist x = 1",
            "SET @a = (SELECT 1, 2), @@persist_only.x = 1",
            "SET global TRANSACTION ISOLATION LEVEL READ COMMITTED",
            "SET DEFAULT ROLE ALL TO u",
            "SET RESOURCE GROUP rg FOR 12",
            "SET STATEMENT max_statement_time = 1 FOR DELETE FROM t",
            "SET NAMES utf8 /*!, GLOBAL x = 1 */",
        ] {
            assert!(!other_whitelisted(s, MY), "{s}");
            assert_eq!(columns(s, MY), [C, B, A], "{s}");
        }
        for s in [
            "SET @x = @@global.max_connections",
            "SET @a = 'GLOBAL'",
            "SET @@session.sql_mode = '', @@local.x = 1, LOCAL y = 2",
            "SET CHARACTER SET utf8",
            "SET ROLE ALL",
            "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE",
        ] {
            assert!(other_whitelisted(s, MY), "{s}");
            assert_eq!(columns(s, MY), [A, A, A], "{s}");
        }
    }

    #[test]
    fn positional_keywords_do_not_skip_quoted_tokens() {
        // `leading_keywords` skips quoted tokens; the whitelist must not.
        assert_eq!(columns("ROLLBACK PREPARED 'x'", PG), [C, B, A]);
        assert_eq!(columns("ROLLBACK WORK TO SAVEPOINT s", MY), [A, A, A]);
        assert_eq!(columns("PREPARE TRANSACTION 'x'", PG), [C, B, A]);
        assert_eq!(columns("COMMIT PREPARED 'x'", PG), [C, B, A]);
        // `SET 'x' GLOBAL …` must not read as a session `SET`.
        assert!(!other_whitelisted("SET `a` GLOBAL x = 1", MY));
    }

    #[test]
    fn lifts_read_only_variants() {
        for (s, d) in [
            ("SET TRANSACTION READ WRITE", MY),
            ("SET GLOBAL TRANSACTION READ WRITE", MY),
            (
                "SET SESSION TRANSACTION ISOLATION LEVEL READ COMMITTED, READ WRITE",
                MY,
            ),
            ("set @@session.transaction_read_only = 0", MY),
            ("SET @@tx_read_only := 'OFF'", MY),
            ("SET NAMES utf8, LOCAL transaction_read_only = false", MY),
            ("SET SESSION transaction_read_only = DEFAULT", MY),
            ("/*!40101 SET tx_read_only = 0 */", MY),
            ("START TRANSACTION READ WRITE", MY),
            ("BEGIN READ WRITE", PG),
            (
                "START TRANSACTION ISOLATION LEVEL SERIALIZABLE, READ WRITE",
                PG,
            ),
            ("SET TRANSACTION READ WRITE", PG),
            ("SET LOCAL transaction_read_only = off", PG),
            ("SET default_transaction_read_only = 'off'", PG),
            ("SET default_transaction_read_only TO DEFAULT", PG),
            ("RESET default_transaction_read_only", PG),
            ("reset all", PG),
            ("DISCARD ALL", PG),
        ] {
            assert!(lifts_read_only(s, d), "{s} ({d:?})");
            let v = verdict(s, classify(s, d), d, None, true);
            assert_eq!(v, B, "{s} ({d:?})");
        }
        for (s, d) in [
            ("SET SESSION TRANSACTION READ ONLY", MY),
            ("SET tx_read_only = 1", MY),
            ("SET transaction_read_only = ON", MY),
            ("SET @a = 'read write'", MY),
            ("START TRANSACTION READ ONLY", MY),
            ("BEGIN", PG),
            ("SET default_transaction_read_only = on", PG),
            ("DISCARD PLANS", PG),
            ("RESET MASTER", MY),
            ("SET search_path = x", PG),
        ] {
            assert!(!lifts_read_only(s, d), "{s} ({d:?})");
        }
    }

    fn ro(s: &str, d: Dialect) -> Verdict {
        verdict(s, classify(s, d), d, None, true)
    }

    #[test]
    fn quoted_read_only_names_still_lift() {
        for (s, d) in [
            (r#"SET "default_transaction_read_only" = off"#, PG),
            (r#"SET LOCAL "Transaction_Read_Only" TO false"#, PG),
            ("SET SESSION `transaction_read_only` = 0", MY),
            ("SET @@session.`tx_read_only` = 0", MY),
            ("SET @@`tx_read_only` = 0", MY),
            ("SET NAMES utf8, `tx_read_only` = 0", MY),
            // Any other name shape counts as lifting.
            ("SET (x) = 1", PG),
            ("SET @@session.(x) = 1", MY),
        ] {
            assert!(lifts_read_only(s, d), "{s} ({d:?})");
            assert_eq!(ro(s, d), B, "{s} ({d:?})");
        }
        for (s, d) in [
            (r#"SET "search_path" = x"#, PG),
            ("SET `tx_read_only` = 1", MY),
            (r#"SET "default_transaction_read_only" = 'on'"#, PG),
            ("SET @'x' = 0", MY),
        ] {
            assert!(!lifts_read_only(s, d), "{s} ({d:?})");
        }
    }

    #[test]
    fn pg_set_config_lifts() {
        for s in [
            "SELECT set_config('default_transaction_read_only','off',false)",
            "DECLARE c CURSOR FOR SELECT set_config('transaction_read_only','off',true)",
            "SELECT set_config(name, 'off', false) FROM t",
            "SELECT pg_catalog.set_config('Transaction_Read_Only', 'off', false)",
            r#"SELECT "set_config"('default_transaction_read_only', 'off', false)"#,
            "SELECT SET_CONFIG ($$default_transaction_read_only$$, 'off', false)",
            "SELECT set_config($q$transaction_read_only$q$, 'off', false)",
            "SELECT set_config(E'transaction_read_only', 'off', false)",
            "SELECT set_config('default_' || 'transaction_read_only', 'off', false)",
            "SELECT set_config($1, 'off', false)",
            "SELECT set_config(('transaction_read_only'), 'off', false)",
            "SELECT 1 FROM t WHERE set_config('transaction_read_only', 'off', false) IS NULL",
        ] {
            assert!(lifts_read_only(s, PG), "{s}");
            assert_eq!(ro(s, PG), B, "{s}");
        }
        for s in [
            "SELECT set_config('application_name','x',false)",
            "SELECT 'set_config(''transaction_read_only'', ''off'', false)'",
            "SELECT current_setting('transaction_read_only')",
            "SELECT set_config",
        ] {
            assert!(!lifts_read_only(s, PG), "{s}");
            assert_eq!(ro(s, PG), A, "{s}");
        }
        // MySQL has no set_config.
        let s = "SELECT set_config('default_transaction_read_only','off',false)";
        assert_eq!(ro(s, MY), A);
        // Not read-only: an ordinary read.
        let p = verdict(s, classify(s, PG), PG, Some(EnvTag::Production), false);
        assert_eq!(p, A);
    }

    #[test]
    fn typed_explain_analyze_matches_the_analyze_flag() {
        for (s, d) in [
            ("EXPLAIN ANALYZE SELECT 1", MY),
            ("explain analyze format=tree SELECT 1", MY),
            ("DESC ANALYZE SELECT 1", MY),
            ("EXPLAIN ANALYZE SELECT 1", PG),
            ("EXPLAIN ANALYSE VERBOSE SELECT 1", PG),
            ("EXPLAIN (ANALYZE, BUFFERS) SELECT 1", PG),
            ("EXPLAIN (BUFFERS, ANALYZE true) SELECT 1", PG),
            ("EXPLAIN (analyse on, FORMAT JSON) TABLE t", PG),
        ] {
            assert_eq!(columns(s, d), [C, A, A], "{s} ({d:?})");
            assert_eq!(plan_columns(s, d, false, false), [C, A, A], "{s} ({d:?})");
        }
        for (s, d) in [
            ("EXPLAIN SELECT 1", MY),
            ("EXPLAIN FORMAT=JSON SELECT 1", MY),
            ("EXPLAIN SELECT 1", PG),
            ("EXPLAIN (ANALYZE false) SELECT 1", PG),
            ("EXPLAIN (ANALYZE off, FORMAT JSON) SELECT 1", PG),
            ("EXPLAIN (FORMAT JSON) SELECT 1", PG),
        ] {
            assert_eq!(columns(s, d), [A, A, A], "{s} ({d:?})");
        }
        // The analyzed statement's own kind still applies.
        for d in BOTH {
            assert_eq!(columns("EXPLAIN ANALYZE DELETE FROM t", *d), [C, B, A]);
        }
    }

    #[test]
    fn kind_can_only_make_the_verdict_stricter() {
        // A caller-supplied kind stricter than the text's readings is honoured.
        let s = "SET NAMES utf8";
        let v = verdict(s, StatementKind::Ddl, MY, Some(EnvTag::Production), false);
        assert_eq!(v, C);
    }

    #[test]
    fn explain_plain_is_always_allowed() {
        assert_eq!(plan_columns("DELETE FROM t", MY, true, false), [A, A, A]);
        assert_eq!(plan_columns("DELETE FROM t", PG, true, false), [A, A, A]);
        // Plain explain covers every kind except Ddl (§5.1).
        assert_eq!(plan_columns("DROP TABLE t", MY, true, false), [C, B, A]);
    }

    #[test]
    fn explain_analyze_confirms_on_production_only() {
        for d in BOTH {
            assert_eq!(plan_columns("SELECT 1", *d, true, true), [C, A, A]);
            assert_eq!(plan_columns("SELECT 1", *d, false, true), [C, A, A]);
            // Should a non-Read reach the guard, the stricter rule still applies.
            assert_eq!(plan_columns("DELETE FROM t", *d, true, true), [C, B, A]);
        }
    }

    const MYSQLDUMP: &str = "-- MySQL dump 10.13  Distrib 8.0.36, for Linux (x86_64)
--
-- Host: localhost    Database: shop
-- ------------------------------------------------------
-- Server version\t8.0.36

/*!40101 SET @OLD_CHARACTER_SET_CLIENT=@@CHARACTER_SET_CLIENT */;
/*!40101 SET @OLD_CHARACTER_SET_RESULTS=@@CHARACTER_SET_RESULTS */;
/*!40101 SET @OLD_COLLATION_CONNECTION=@@COLLATION_CONNECTION */;
/*!50503 SET NAMES utf8mb4 */;
/*!40103 SET @OLD_TIME_ZONE=@@TIME_ZONE */;
/*!40103 SET TIME_ZONE='+00:00' */;
/*!40014 SET @OLD_UNIQUE_CHECKS=@@UNIQUE_CHECKS, UNIQUE_CHECKS=0 */;
/*!40014 SET @OLD_FOREIGN_KEY_CHECKS=@@FOREIGN_KEY_CHECKS, FOREIGN_KEY_CHECKS=0 */;
/*!40101 SET @OLD_SQL_MODE=@@SQL_MODE, SQL_MODE='NO_AUTO_VALUE_ON_ZERO' */;
/*!40111 SET @OLD_SQL_NOTES=@@SQL_NOTES, SQL_NOTES=0 */;
SET @MYSQLDUMP_TEMP_LOG_BIN = @@SESSION.SQL_LOG_BIN;
SET @@SESSION.SQL_LOG_BIN= 0;

--
-- GTID state at the beginning of the backup
--

SET @@GLOBAL.GTID_PURGED=/*!80000 '+'*/ '3e11fa47-71ca-11e1-9e33-c80aa9429562:1-5';

--
-- Table structure for table `t`
--

DROP TABLE IF EXISTS `t`;
/*!40101 SET @saved_cs_client     = @@character_set_client */;
/*!50503 SET character_set_client = utf8mb4 */;
CREATE TABLE `t` (
  `id` int NOT NULL,
  `name` varchar(20) DEFAULT NULL,
  PRIMARY KEY (`id`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
/*!40101 SET character_set_client = @saved_cs_client */;

--
-- Dumping data for table `t`
--

LOCK TABLES `t` WRITE;
/*!40000 ALTER TABLE `t` DISABLE KEYS */;
INSERT INTO `t` VALUES (1,'a;b'),(2,'Tiếng Việt 😀');
/*!40000 ALTER TABLE `t` ENABLE KEYS */;
UNLOCK TABLES;
SET @@SESSION.SQL_LOG_BIN = @MYSQLDUMP_TEMP_LOG_BIN;
/*!40103 SET TIME_ZONE=@OLD_TIME_ZONE */;

/*!40101 SET SQL_MODE=@OLD_SQL_MODE */;
/*!40014 SET FOREIGN_KEY_CHECKS=@OLD_FOREIGN_KEY_CHECKS */;
/*!40014 SET UNIQUE_CHECKS=@OLD_UNIQUE_CHECKS */;
/*!40101 SET CHARACTER_SET_CLIENT=@OLD_CHARACTER_SET_CLIENT */;
/*!40101 SET CHARACTER_SET_RESULTS=@OLD_CHARACTER_SET_RESULTS */;
/*!40101 SET COLLATION_CONNECTION=@OLD_COLLATION_CONNECTION */;
/*!40111 SET SQL_NOTES=@OLD_SQL_NOTES */;

-- Dump completed on 2026-10-20 10:00:00
";

    #[test]
    fn mysqldump_script_on_production() {
        let mut p = plan(MYSQLDUMP, Target::All, MY);
        guard(&mut p, Some(EnvTag::Production), false, false);
        assert_eq!(p.stmts.len(), 32);
        let mut seen = [0usize; 6];
        for s in &p.stmts {
            let t = s.text.as_str();
            let want = if t.starts_with("SET @@GLOBAL.") {
                seen[0] += 1;
                C
            } else if t.contains("SET ") {
                seen[1] += 1;
                A
            } else if t.starts_with("LOCK TABLES `t` WRITE") {
                seen[2] += 1;
                C
            } else if t.starts_with("/*!40000 ALTER TABLE `t`") {
                seen[3] += 1;
                C
            } else if t.starts_with("INSERT") || t.starts_with("UNLOCK TABLES") {
                seen[4] += 1;
                C
            } else {
                // DROP TABLE / CREATE TABLE
                seen[5] += 1;
                C
            };
            assert_eq!(s.verdict, want, "{t}");
        }
        assert_eq!(seen, [1, 24, 1, 2, 2, 2]);

        // The same script on a read-only connection: every non-SET is blocked.
        let mut p = plan(MYSQLDUMP, Target::All, MY);
        guard(&mut p, Some(EnvTag::Production), true, false);
        for s in &p.stmts {
            let session_set = s.text.contains("SET ") && !s.text.starts_with("SET @@GLOBAL.");
            assert_eq!(s.verdict, if session_set { A } else { B }, "{}", s.text);
        }
    }

    /// A forgotten `DELIMITER ;`: under `$$` one planned statement holds two
    /// statements, which the server runs as such (multi-statements are on).
    const FORGOTTEN_DELIMITER: &str = "DELIMITER $$\nSELECT * FROM t; DELETE FROM t WHERE id = 1;";

    #[test]
    fn statement_with_depth0_semicolons_is_judged_on_every_piece() {
        let p = plan(FORGOTTEN_DELIMITER, Target::All, MY);
        assert_eq!(p.stmts.len(), 1);
        // `plan` gives it the strictest kind of its pieces.
        assert_eq!(p.stmts[0].kind, StatementKind::Write);
        assert_eq!(
            plan_columns(FORGOTTEN_DELIMITER, MY, false, false),
            [C, B, A]
        );
        // Pieces whose leading statement is whitelisted are still judged.
        for s in [
            "SET NAMES utf8; DELETE FROM t",
            "SELECT 1; DROP TABLE t",
            "USE shop; SET GLOBAL x = 1",
            "/*!40101 SELECT 1; DELETE FROM t */",
        ] {
            assert_eq!(columns(s, MY), [C, B, A], "{s}");
        }
        // Read-only lifting in a later piece blocks.
        assert_eq!(columns("SELECT 1; SET tx_read_only = 0", MY), [A, B, A]);
        // Every piece a read: unchanged.
        assert_eq!(columns("SELECT 1; SELECT 2;", MY), [A, A, A]);
    }

    const DUMP_PROCEDURE: &str = "DELIMITER $$
CREATE DEFINER=`root`@`localhost` PROCEDURE `p`(IN x INT)
BEGIN
  SELECT * FROM t WHERE id = x;
  UPDATE t SET n = n + 1 WHERE id = x;
END$$
DELIMITER ;";

    #[test]
    fn compound_bodies_keep_their_own_verdict() {
        let p = plan(DUMP_PROCEDURE, Target::All, MY);
        assert_eq!(p.stmts.len(), 1);
        assert_eq!(p.stmts[0].kind, StatementKind::Ddl);
        assert!(!resplits(&p.stmts[0].text, MY));
        // Ddl: Confirm on Production, Block read-only, Allow elsewhere.
        assert_eq!(plan_columns(DUMP_PROCEDURE, MY, false, false), [C, B, A]);
        for s in [
            "CREATE PROCEDURE p() BEGIN SELECT 1; DELETE FROM t; END",
            "create or replace definer = current_user() function f() returns int begin return 1; end",
            "CREATE DEFINER=root@localhost TRIGGER tr BEFORE INSERT ON t FOR EACH ROW BEGIN SET NEW.a = 1; END",
            "CREATE DEFINER='u'@'%' EVENT e ON SCHEDULE EVERY 1 DAY DO BEGIN DELETE FROM t; END",
            "CREATE OR REPLACE AGGREGATE FUNCTION f(x INT) RETURNS INT BEGIN RETURN 1; END",
            "/*!50003 CREATE*/ /*!50017 DEFINER=`root`@`localhost`*/ /*!50003 TRIGGER tr BEFORE INSERT ON t FOR EACH ROW BEGIN SET NEW.a = 1; END */",
        ] {
            assert!(!resplits(s, MY), "{s}");
            assert_eq!(columns(s, MY), [C, B, A], "{s}");
        }
        let s = "BEGIN NOT ATOMIC SELECT 1; SELECT 2; END";
        assert!(!resplits(s, MY));
        assert_eq!(columns(s, MY), [C, B, A]);
        // Anything else with a depth-0 `;` re-splits.
        for s in [
            "CREATE TABLE t (a int); DELETE FROM t",
            "CREATE VIEW v AS SELECT 1; DELETE FROM t",
            FORGOTTEN_DELIMITER.trim_start_matches("DELIMITER $$\n"),
        ] {
            assert!(resplits(s, MY), "{s}");
        }
    }

    #[test]
    fn plain_explain_of_several_pieces_is_judged_as_a_run() {
        // Plain Explain allows everything but Ddl, unless the text is several
        // statements: then it is judged like any run.
        let text = "DELIMITER $$\nSELECT 1; DELETE FROM t";
        assert_eq!(plan_columns(text, MY, true, false), [C, B, A]);
    }

    #[test]
    fn pg_statements_never_hold_a_depth0_semicolon() {
        for text in [
            "SELECT 1; DELETE FROM t",
            "DELIMITER $$\nSELECT 1; DELETE FROM t;",
            "create function f() returns int as $$ begin return 1; end $$ language plpgsql; select 1",
            "select $tag$ ; $$ ; $tag$; select E'a\\';b'; /* a /* b; */ c; */ select 2",
            "SELECT ';'; SELECT \"a;b\"",
            "DO $$ BEGIN PERFORM 1; END $$; SELECT 1",
        ] {
            let p = plan(text, Target::All, PG);
            for s in &p.stmts {
                assert_eq!(split(&s.text, PG).len(), 1, "{text:?}: {}", s.text);
                assert!(!resplits(&s.text, PG), "{}", s.text);
            }
        }
    }

    #[test]
    fn guard_is_fast_on_large_scripts() {
        let text = "/*!40101 SET NAMES utf8 */;\nINSERT INTO t VALUES (1, 'ư');\n".repeat(5_000);
        let started = std::time::Instant::now();
        let mut p = plan(&text, Target::All, MY);
        guard(&mut p, Some(EnvTag::Production), false, false);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert_eq!(p.stmts.iter().filter(|s| s.verdict == C).count(), 5_000);
    }
}
