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
//! On top of that table, a statement that runs and changes every row of a table
//! ([`crate::unbounded`]: `UPDATE`/`DELETE` without WHERE or with an always-true one,
//! `TRUNCATE`) is at least Confirm: always on Production and Staging, and on Local and
//! untagged connections while [`GuardCtx::confirm_unbounded`] (the "Confirm UPDATE/DELETE
//! without WHERE" setting, on by default) is set. A plain Explain runs nothing and is not
//! affected; read-only Block stays Block.
//!
//! Spec §4.3 adds ([`crate::risk`]): a `DROP` of an object is Confirm on Staging;
//! `DROP DATABASE` / `DROP SCHEMA` is Confirm on every connection; and on
//! Production a Confirm statement that changes every row of a table or drops something
//! carries [`crate::plan::PlannedStmt::confirm_name`], the name the user must type.
//!
//! A planned statement that holds several statements for the server (a MySQL
//! `DELIMITER` left on, see [`resplits`]) gets the strictest verdict over all of
//! them, and plain Explain does not cover it. When one of those statements is a `SET`
//! that changes how the server lexes the rest (`sql_mode`, `NAMES`, see
//! [`changes_lexing`]) and others follow it, the server reads them differently from
//! the split judged here, so the verdict is at least Confirm (Block on read-only).
//!
//! Keyword positions are read from [`tokenize`], which keeps quoted tokens in
//! place, so `SET \`a\` GLOBAL …` never passes for a session `SET`.

use dnt_core::config::EnvTag;
use dnt_core::sql::{
    classify, pg_backslash_reading, readings, resplit_reading, tokenize, Dialect, StatementKind,
    TokKind, Token,
};

use crate::plan::{ScriptPlan, Verdict};
use crate::risk::DropKind;

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
///
/// Unbounded writes are confirmed on every connection (the setting's default); use
/// [`guard_with`] to pass the setting.
pub fn guard(plan: &mut ScriptPlan, env: Option<EnvTag>, read_only: bool, analyze: bool) {
    guard_with(
        plan,
        GuardCtx {
            env,
            read_only,
            analyze,
            confirm_unbounded: true,
        },
    );
}

/// [`guard`] with every input given.
pub fn guard_with(plan: &mut ScriptPlan, ctx: GuardCtx) {
    let (dialect, explain) = (plan.dialect, plan.explain);
    for s in &mut plan.stmts {
        s.verdict = stmt_verdict(&s.text, s.kind, dialect, explain, ctx);
        // Production asks the name of what a risky statement changes or drops.
        s.confirm_name = if ctx.env == Some(EnvTag::Production) && s.verdict == Verdict::Confirm {
            crate::risk::confirm_name(&s.unbounded, &s.drops)
        } else {
            None
        };
    }
    plan.guard = Some(ctx);
}

/// What [`guard`] judged a plan with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuardCtx {
    pub env: Option<EnvTag>,
    pub read_only: bool,
    pub analyze: bool,
    /// Confirm unbounded writes on Local and untagged connections (the "Confirm
    /// UPDATE/DELETE without WHERE" setting). Production and Staging always confirm them.
    pub confirm_unbounded: bool,
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
        confirm_unbounded,
    } = ctx;
    let v = if analyze {
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
        // one reach here. A plain Explain runs nothing.
        return Verdict::Allow;
    } else {
        verdict(text, kind, dialect, env, read_only)
    };
    if v != Verdict::Allow {
        return v;
    }
    // §4.3: a write that changes every row of a table (UPDATE / DELETE without WHERE,
    // TRUNCATE) is confirmed on Production and Staging whatever the setting; the setting
    // decides on Local and untagged connections (R43).
    let writes = crate::unbounded::unbounded_writes(text, dialect);
    let tagged = matches!(env, Some(EnvTag::Production | EnvTag::Staging));
    if !writes.is_empty() && (confirm_unbounded || tagged) {
        return Verdict::Confirm;
    }
    // §4.3: a DROP of an object is confirmed on Staging (Production confirms it as DDL
    // already); DROP DATABASE / SCHEMA everywhere.
    if mentions_drop(text) {
        let drops = crate::risk::drop_targets(text, dialect);
        let staging_object =
            env == Some(EnvTag::Staging) && drops.iter().any(|d| d.kind == DropKind::Object);
        let database = drops
            .iter()
            .any(|d| matches!(d.kind, DropKind::Database | DropKind::Schema));
        if staging_object || database {
            return Verdict::Confirm;
        }
    }
    v
}

/// A cheap pre-check before lexing for `DROP`: whether `text` holds the letters.
fn mentions_drop(text: &str) -> bool {
    text.as_bytes()
        .windows(4)
        .any(|w| w.eq_ignore_ascii_case(b"drop"))
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
    for r in guard_readings(stmt, dialect) {
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
        let pieces = resplit_reading(&r, &toks, dialect).unwrap_or_default();
        let piece_toks: Vec<_> = pieces.iter().map(|p| tokenize(p, dialect)).collect();
        for (n, (piece, ptoks)) in pieces.iter().zip(&piece_toks).enumerate() {
            if ptoks.is_empty() {
                continue;
            }
            v = strictest(v, judge(piece, ptoks));
            // The server lexes the pieces after a `SET sql_mode` / `SET NAMES` with the
            // new setting, not as they were split here, so they are not proven harmless:
            // `SET sql_mode='NO_BACKSLASH_ESCAPES'; SELECT 'x\'; DROP TABLE t; #'`.
            let more = piece_toks[n + 1..].iter().any(|t| !t.is_empty());
            if more && changes_lexing(piece, ptoks, dialect) {
                v = strictest(v, unsafe_verdict);
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
    for r in guard_readings(stmt, dialect) {
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
    guard_readings(stmt, dialect)
        .iter()
        .any(|r| lifts_toks(r, &tokenize(r, dialect), dialect))
}

/// Every way the server may read `stmt` that the guard judges: its [`readings`]
/// (MySQL executable comments) and, on PostgreSQL, the reading of a server with
/// `standard_conforming_strings = off` ([`pg_backslash_reading`]) when it differs.
/// The setting may be off on the server or turned off by the session, and the
/// lexer cannot know which, so a statement must pass under both: otherwise
/// `WITH a AS (SELECT 'x\''), d AS (DELETE …) SELECT 1 /* ' */` would hide its
/// `DELETE` inside what the standard reading takes for a string.
fn guard_readings(stmt: &str, dialect: Dialect) -> Vec<String> {
    let mut out = readings(stmt, dialect);
    if !dialect.is_mysql() {
        out.extend(pg_backslash_reading(stmt));
    }
    out
}

/// MySQL: whether `toks` (one statement of `src`) is a `SET` that may change how the
/// server lexes the statements after it: it names `sql_mode` (`ANSI_QUOTES`,
/// `NO_BACKSLASH_ESCAPES`), `NAMES`, `CHARACTER SET` / `CHARSET` or a `character_set_*`
/// variable (a multibyte client charset such as GBK can swallow a `\`), as a word or
/// quoted. A quoted token it cannot read counts too.
fn changes_lexing(src: &str, toks: &[Token], dialect: Dialect) -> bool {
    if !dialect.is_mysql() || !is_word(toks, 0, "SET") {
        return false;
    }
    let lexing = |name: &str| {
        let n = name.trim_start_matches('@').to_ascii_lowercase();
        n.ends_with("sql_mode")
            || n.contains("character_set")
            || ["names", "character", "charset"].contains(&n.as_str())
    };
    toks.iter().any(|t| match t.kind {
        TokKind::Word(w) => lexing(w),
        TokKind::Quoted => unquote(&src[t.start..t.end]).is_none_or(|n| lexing(&n)),
        _ => false,
    })
}

/// Whether `stmt` (one planned statement) holds more than one statement for the
/// server (moved to `dnt_core::sql` so the MySQL driver can use it too).
pub use dnt_core::sql::resplits;

/// The kinds of the pieces of `stmt` ([`resplits`]), reading by reading, each in
/// order; empty when it does not re-split.
pub(crate) fn piece_kinds(stmt: &str, dialect: Dialect) -> Vec<StatementKind> {
    readings(stmt, dialect)
        .iter()
        .filter_map(|r| resplit_reading(r, &tokenize(r, dialect), dialect))
        .flatten()
        .map(|p| classify(&p, dialect))
        .collect()
}

fn rank(v: Verdict) -> u8 {
    match v {
        Verdict::Allow => 0,
        Verdict::Confirm => 1,
        Verdict::Block => 2,
    }
}

pub(crate) fn strictest(a: Verdict, b: Verdict) -> Verdict {
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
/// `'…'`, `"…"`, `` `…` `` (a doubled quote stands for one), `E'…'` (its escapes
/// decoded as the server does, see [`decode_e_string`]) and `$tag$…$tag$`.
pub(crate) fn unquote(q: &str) -> Option<String> {
    let b = q.as_bytes();
    match *b.first()? {
        c @ (b'\'' | b'"' | b'`') => (q.len() >= 2 && b[q.len() - 1] == c).then(|| {
            let c = char::from(c);
            q[1..q.len() - 1].replace(&format!("{c}{c}"), &c.to_string())
        }),
        b'E' | b'e' => decode_e_string(&q[1..]),
        b'$' => {
            let tag_len = q[1..].find('$')? + 2;
            let tag = &q[..tag_len];
            (q.len() >= 2 * tag_len && q.ends_with(tag))
                .then(|| q[tag_len..q.len() - tag_len].to_string())
        }
        _ => None,
    }
}

/// The value of a PostgreSQL escape string `'…'` (the token after its `E`), as the
/// server reads it: `\b` `\f` `\n` `\r` `\t`, `\ooo` (1–3 octal digits), `\xhh` (1–2
/// hex digits), `\uXXXX`, `\UXXXXXXXX`, any other `\c` is `c`, and `''` is `'`.
/// `None` when it is unterminated, ends in a lone `\`, or decodes to invalid UTF-8 or
/// a bad code point; callers treat that as a name they cannot prove harmless.
fn decode_e_string(q: &str) -> Option<String> {
    let body = q.strip_prefix('\'')?.strip_suffix('\'')?;
    let b = body.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let digits = |from: usize, max: usize, radix: u32| -> (u32, usize) {
        let mut v = 0u32;
        let mut n = 0;
        while n < max {
            match b.get(from + n).and_then(|&c| char::from(c).to_digit(radix)) {
                Some(d) => v = v * radix + d,
                None => break,
            }
            n += 1;
        }
        (v, n)
    };
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' => {
                let c = *b.get(i + 1)?;
                i += 2;
                match c {
                    b'b' => out.push(0x08),
                    b'f' => out.push(0x0c),
                    b'n' => out.push(b'\n'),
                    b'r' => out.push(b'\r'),
                    b't' => out.push(b'\t'),
                    b'0'..=b'7' => {
                        let (v, n) = digits(i - 1, 3, 8);
                        out.push(v as u8);
                        i += n - 1;
                    }
                    b'x' => match digits(i, 2, 16) {
                        (_, 0) => out.push(b'x'),
                        (v, n) => {
                            out.push(v as u8);
                            i += n;
                        }
                    },
                    b'u' | b'U' => {
                        let len = if c == b'u' { 4 } else { 8 };
                        let (v, n) = digits(i, len, 16);
                        if n != len {
                            return None;
                        }
                        let ch = char::from_u32(v)?;
                        out.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
                        i += n;
                    }
                    other => out.push(other),
                }
            }
            // A doubled quote stands for one (a lone one cannot occur in the token).
            b'\'' => {
                out.push(b'\'');
                i += if b.get(i + 1) == Some(&b'\'') { 2 } else { 1 };
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// PG: whether token `i` is a `U&'…'` / `U&"…"` literal or name, whose `\XXXX`
/// escapes (and `UESCAPE` character) this guard does not decode.
fn unicode_escaped(toks: &[Token], i: usize) -> bool {
    i >= 2
        && toks[i].kind == TokKind::Quoted
        && toks[i - 1].kind == TokKind::Punct(b'&')
        && word(toks, i - 2).is_some_and(|w| w.eq_ignore_ascii_case("U"))
        && toks[i - 2].end == toks[i - 1].start
        && toks[i - 1].end == toks[i].start
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
pub(crate) fn explain_analyze(toks: &[Token], dialect: Dialect) -> bool {
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
                // `U&"set\005Fconfig"`: not decoded, so it may be `set_config`.
                q.starts_with('"')
                    && (unicode_escaped(toks, i)
                        || unquote(q).is_some_and(|n| n.eq_ignore_ascii_case("set_config")))
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
    // PG `U&"…"`: a name this guard does not decode may be a read-only switch.
    if is_word(a, i, "U") && is_punct(a, i + 1, b'&') {
        return true;
    }
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

    /// Escapes the server decodes must not hide a read-only switch: `E'…'` is decoded
    /// as PostgreSQL does, and `U&` names / literals (not decoded) count as lifting.
    #[test]
    fn escaped_read_only_names_still_lift() {
        for s in [
            r"SELECT set_config(E'default\x5ftransaction_read_only','off',false)",
            r"SELECT set_config(E'default\137transaction\137read\137only','off',false)",
            r"SELECT set_config(E'\u0074ransaction_read_only','off',false)",
            r"SELECT set_config(E'transaction\_read\_only','off',false)",
            r"SELECT set_config(e'TRANSACTION_READ_ONLY','off',false)",
            // Not decodable: an invalid code point.
            r"SELECT set_config(E'\uD800', 'off', false)",
            r#"SELECT U&"set\005Fconfig"('default_transaction_read_only','off',false)"#,
            r"SELECT set_config(U&'transaction\005Fread\005Fonly','off',false)",
            r#"SET U&"default\005Ftransaction_read_only" = off"#,
            r#"SET SESSION U&"transaction\005Fread\005Fonly" TO off"#,
        ] {
            assert!(lifts_read_only(s, PG), "{s}");
            assert_eq!(ro(s, PG), B, "{s}");
        }
        for s in [
            r"SELECT set_config(E'application\x5fname','x',false)",
            r"SELECT E'default\x5ftransaction_read_only'",
            r#"SELECT U&"d\0061ta" FROM t"#,
        ] {
            assert!(!lifts_read_only(s, PG), "{s}");
            assert_eq!(ro(s, PG), A, "{s}");
        }
    }

    #[test]
    fn e_strings_decode_like_the_server() {
        for (q, want) in [
            (r"'a\x5fb'", Some("a_b")),
            (r"'\137'", Some("_")),
            (r"'\1a'", Some("\u{1}a")),
            (r"'\xg'", Some("xg")),
            (r"'\b\f\n\r\t'", Some("\u{8}\u{c}\n\r\t")),
            (r"'\u00e9\U0001F600'", Some("é😀")),
            (r"'it''s \' \\ \q'", Some(r"it's ' \ q")),
            (r"'\xc3\xa9'", Some("é")),
            (r"'\xff'", None),
            (r"'\u12'", None),
            (r"'a\", None),
            (r"'a", None),
        ] {
            assert_eq!(decode_e_string(q).as_deref(), want, "{q}");
        }
    }

    /// `standard_conforming_strings` can be off on the server or turned off by the
    /// session (`SET` is whitelisted), and then `'x\''` ends after `x'`. A statement
    /// must pass under that reading too, or a write hides in what looks like a string.
    #[test]
    fn writes_hidden_by_backslash_strings_are_judged() {
        let prod = |s: &str| verdict(s, classify(s, PG), PG, Some(EnvTag::Production), false);
        for s in [
            r"WITH a AS (SELECT 'x\''), d AS (DELETE FROM t RETURNING 1) SELECT 1 /* ' ) SELECT 1 */",
            r"SELECT 'a\''; DELETE FROM t; SELECT '''",
        ] {
            // The standard reading alone sees only a read.
            assert_eq!(classify(s, PG), StatementKind::Read, "{s}");
            assert_eq!(ro(s, PG), B, "{s}");
            assert_eq!(prod(s), C, "{s}");
        }
        // The read-only switch hidden the same way.
        let s = r"SELECT 'x\'', set_config('transaction_read_only', 'off', false), '";
        assert!(lifts_read_only(s, PG), "{s}");
        assert_eq!(ro(s, PG), B, "{s}");
        // Backslashes that change nothing keep reads allowed.
        for s in [
            r"SELECT 'C:\temp\new' AS path",
            r"SELECT 'a\b', E'\n', $$x\'$$ FROM t",
            r"SELECT 'it''s'",
        ] {
            assert_eq!(ro(s, PG), A, "{s}");
            assert_eq!(prod(s), A, "{s}");
        }
        // MySQL is lexed with the session's tracked sql_mode, not twice.
        let s = r"SELECT 'it\'s; DELETE x'";
        assert_eq!(ro(s, MY), A);
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
        // MySQL `DESC` / `DESCRIBE` are `EXPLAIN`: read-only blocks the analyzed write.
        for s in [
            "DESC ANALYZE DELETE t1 FROM t1 JOIN t2 ON t1.a = t2.a",
            "describe analyze format=tree UPDATE t SET a = 1 WHERE id = 1",
        ] {
            assert_eq!(columns(s, MY), [C, B, A], "{s}");
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
            assert_eq!(
                plan_columns("DELETE FROM t WHERE id = 1", *d, true, true),
                [C, B, A]
            );
            // Explain Analyze runs it: an unbounded write is confirmed everywhere.
            assert_eq!(plan_columns("DELETE FROM t", *d, true, true), [C, B, C]);
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

    /// The server lexes the pieces after a `SET sql_mode` / `SET NAMES` with the new
    /// setting: under `NO_BACKSLASH_ESCAPES` the string below ends at `\'` and the DROP
    /// runs, though the split judged here holds a SELECT of one string.
    #[test]
    fn pieces_after_a_lexing_change_are_not_proven_harmless() {
        let attack = "DELIMITER //\nSET sql_mode='NO_BACKSLASH_ESCAPES'; SELECT 'x\\'; DROP TABLE orders; #'//";
        let p = plan(attack, Target::All, MY);
        assert_eq!(p.stmts.len(), 1);
        assert_eq!(plan_columns(attack, MY, false, false), [C, B, A]);
        for s in [
            r#"SET sql_mode = 'ANSI_QUOTES'; SELECT "x"; SELECT 1"#,
            "set session sql_mode = ''; SELECT 1",
            "SET @@session.sql_mode = 'NO_BACKSLASH_ESCAPES'; SELECT 1",
            "SET @@SQL_MODE := 'ANSI'; SELECT 1",
            "SET @@session.`sql_mode` = 'ANSI'; SELECT 1",
            "SET NAMES gbk; SELECT 1",
            "SET CHARACTER SET gbk; SELECT 1",
            "set charset gbk; SELECT 1",
            "SET character_set_client = gbk; SELECT 1",
            "SET autocommit = 1, sql_mode = 'ANSI'; SELECT 1",
            "/*!40101 SET sql_mode='NO_BACKSLASH_ESCAPES' */; SELECT 1",
        ] {
            assert_eq!(columns(s, MY), [C, B, A], "{s}");
        }
        // Nothing runs after it, or the SET changes nothing the lexer depends on.
        for s in [
            "SELECT 1; SET sql_mode = 'ANSI'",
            "SET sql_mode = 'ANSI'; -- done",
            "SET autocommit = 1; SELECT 1",
            "SET @x = 1; SELECT @x",
            "SET time_zone = '+00:00'; SELECT NOW()",
        ] {
            assert_eq!(columns(s, MY), [A, A, A], "{s}");
        }
        // A planned statement of its own: `run` re-judges the rest after the mode change.
        assert_eq!(columns("SET sql_mode = 'ANSI'", MY), [A, A, A]);
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
        let text = "DELIMITER $$\nSELECT 1; DELETE FROM t WHERE id = 1";
        assert_eq!(plan_columns(text, MY, true, false), [C, B, A]);
        // ... including the unbounded-write rule.
        let text = "DELIMITER $$\nSELECT 1; DELETE FROM t";
        assert_eq!(plan_columns(text, MY, true, false), [C, B, C]);
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

    fn judged(text: &str, d: Dialect, ctx: GuardCtx) -> Vec<Verdict> {
        let mut p = plan(text, Target::All, d);
        guard_with(&mut p, ctx);
        assert_eq!(p.guard, Some(ctx));
        p.stmts.iter().map(|s| s.verdict).collect()
    }

    fn ctx(env: Option<EnvTag>, read_only: bool, confirm_unbounded: bool) -> GuardCtx {
        GuardCtx {
            env,
            read_only,
            analyze: false,
            confirm_unbounded,
        }
    }

    /// Environment x read-only x setting, for unbounded and bounded writes.
    #[test]
    fn unbounded_write_matrix() {
        const UNBOUNDED: &[&str] = &[
            "UPDATE t SET a = 1",
            "DELETE FROM t",
            "TRUNCATE t",
            "DELETE FROM t WHERE 1=1",
        ];
        const BOUNDED: &[&str] = &[
            "UPDATE t SET a = 1 WHERE id = 1",
            "DELETE FROM t WHERE id = 2",
        ];
        let envs = [
            Some(EnvTag::Production),
            Some(EnvTag::Staging),
            Some(EnvTag::Local),
            None,
        ];
        for d in BOTH {
            for env in envs {
                let prod = env == Some(EnvTag::Production);
                let staging = env == Some(EnvTag::Staging);
                for setting in [true, false] {
                    for &s in UNBOUNDED {
                        // Production and Staging always confirm (§4.3, R43); the setting
                        // decides on Local and untagged connections only.
                        let want = if prod || staging || setting { C } else { A };
                        assert_eq!(
                            judged(s, *d, ctx(env, false, setting)),
                            [want],
                            "{s} {env:?} {setting}"
                        );
                        // Read-only blocks whatever the setting.
                        assert_eq!(
                            judged(s, *d, ctx(env, true, setting)),
                            [B],
                            "{s} {env:?} ro"
                        );
                    }
                    for &s in BOUNDED {
                        let want = if prod { C } else { A };
                        assert_eq!(
                            judged(s, *d, ctx(env, false, setting)),
                            [want],
                            "{s} {env:?} {setting}"
                        );
                    }
                }
            }
        }
        // `guard` confirms unbounded writes (the setting's default).
        let mut p = plan("DELETE FROM t", Target::All, MY);
        guard(&mut p, Some(EnvTag::Local), false, false);
        assert_eq!(p.stmts[0].verdict, C);
        assert!(p.guard.unwrap().confirm_unbounded);
    }

    #[test]
    fn unbounded_writes_only_flag_their_own_statement() {
        let local = ctx(Some(EnvTag::Local), false, true);
        assert_eq!(
            judged(
                "SELECT 1; DELETE FROM t; DELETE FROM t WHERE id = 1; UPDATE t SET a = 1",
                MY,
                local
            ),
            [A, C, A, C]
        );
        // A WHERE inside a subquery does not bound the statement.
        assert_eq!(
            judged(
                "UPDATE t SET a = (SELECT b FROM u WHERE u.id = t.id)",
                PG,
                local
            ),
            [C]
        );
        // MariaDB `SET STATEMENT … FOR` runs the DELETE: confirmed even off Production.
        assert_eq!(
            judged(
                "SET STATEMENT max_statement_time = 1 FOR DELETE FROM t",
                MY,
                local
            ),
            [C]
        );
        // Routine bodies are not flagged.
        assert!(judged(DUMP_PROCEDURE, MY, local).iter().all(|v| *v == A));
        assert_eq!(
            judged(
                "DELIMITER $$\nCREATE PROCEDURE p() BEGIN DELETE FROM t; END$$",
                MY,
                local
            ),
            [A]
        );
    }

    #[test]
    fn unbounded_write_in_a_delimiter_script() {
        // A forgotten `DELIMITER ;`: the DELETE piece is judged on its own.
        let local = ctx(Some(EnvTag::Local), false, true);
        assert_eq!(judged(FORGOTTEN_DELIMITER, MY, local), [A]);
        assert_eq!(
            judged("DELIMITER $$\nSELECT * FROM t; DELETE FROM t;", MY, local),
            [C]
        );
        assert_eq!(
            judged(
                "DELIMITER $$\nSELECT * FROM t; DELETE FROM t;",
                MY,
                ctx(None, false, false)
            ),
            [A]
        );
    }

    #[test]
    fn plain_explain_of_an_unbounded_write_is_allowed() {
        for setting in [true, false] {
            let mut p = plan("DELETE FROM t", Target::All, PG);
            p.explain = true;
            guard_with(&mut p, ctx(Some(EnvTag::Local), false, setting));
            assert_eq!(p.stmts[0].verdict, A);
            // The flag is still there for the UI.
            assert_eq!(p.stmts[0].unbounded.len(), 1);
        }
    }

    /// Verdict and typed name of a one-statement plan.
    fn judged_named(text: &str, d: Dialect, c: GuardCtx) -> (Verdict, Option<String>) {
        let mut p = plan(text, Target::All, d);
        guard_with(&mut p, c);
        assert_eq!(p.stmts.len(), 1, "{text}");
        (p.stmts[0].verdict, p.stmts[0].confirm_name.clone())
    }

    /// §4.3: DROP / TRUNCATE by env tag (Local, untagged, Staging, Production) and
    /// setting; read-only blocks everything.
    #[test]
    fn drop_matrix_by_env() {
        let n = |s: &str| Some(s.to_string());
        type Row = (&'static str, [(Verdict, Option<&'static str>); 4]);
        // Columns: Local, None, Staging, Production (setting off: only the table decides).
        let rows: &[Row] = &[
            (
                "DROP TABLE IF EXISTS a, `b`",
                [(A, None), (A, None), (C, None), (C, Some("a"))],
            ),
            (
                "DROP VIEW shop.v",
                [(A, None), (A, None), (C, None), (C, Some("shop.v"))],
            ),
            (
                "/*!40000 DROP TABLE t */",
                [(A, None), (A, None), (C, None), (C, Some("t"))],
            ),
            (
                "TRUNCATE `orders`",
                [(A, None), (A, None), (C, None), (C, Some("orders"))],
            ),
            (
                "DROP DATABASE shop",
                [(C, None), (C, None), (C, None), (C, Some("shop"))],
            ),
            (
                "DROP SCHEMA `Sales`",
                [(C, None), (C, None), (C, None), (C, Some("Sales"))],
            ),
            (
                "/*!32312 DROP DATABASE d */",
                [(C, None), (C, None), (C, None), (C, Some("d"))],
            ),
            // Not risky: Production confirms a write but needs no typed name.
            (
                "DELETE FROM t WHERE id = 1",
                [(A, None), (A, None), (A, None), (C, None)],
            ),
            (
                "CREATE TABLE t (a int)",
                [(A, None), (A, None), (A, None), (C, None)],
            ),
            ("SELECT 'DROP DATABASE d'", [(A, None); 4]),
        ];
        let envs = [
            Some(EnvTag::Local),
            None,
            Some(EnvTag::Staging),
            Some(EnvTag::Production),
        ];
        for (s, want) in rows {
            for (env, (v, name)) in envs.iter().zip(want) {
                assert_eq!(
                    judged_named(s, MY, ctx(*env, false, false)),
                    (*v, name.map(str::to_string)),
                    "{s} {env:?}"
                );
                // Read-only blocks whatever the tag, and asks no name.
                if *s != "SELECT 'DROP DATABASE d'" {
                    assert_eq!(
                        judged_named(s, MY, ctx(*env, true, false)),
                        (B, None),
                        "{s} {env:?} ro"
                    );
                }
            }
        }
        // PG quoting.
        assert_eq!(
            judged_named(
                r#"DROP SCHEMA "Sales" CASCADE"#,
                PG,
                ctx(Some(EnvTag::Production), false, true)
            ),
            (C, n("Sales"))
        );
        assert_eq!(
            judged_named("DROP DATABASE d", PG, ctx(None, false, false)),
            (C, None)
        );
        // With the setting on, TRUNCATE is confirmed on Local too (the unbounded rule).
        assert_eq!(
            judged_named("TRUNCATE t", MY, ctx(Some(EnvTag::Local), false, true)),
            (C, None)
        );
        // A DROP in a forgotten-delimiter piece counts.
        assert_eq!(
            judged_named(
                "DELIMITER $$\nSELECT 1; DROP DATABASE d;",
                MY,
                ctx(Some(EnvTag::Local), false, false)
            ),
            (C, None)
        );
        // A routine body's DROP does not.
        assert_eq!(
            judged_named(
                "DELIMITER $$\nCREATE PROCEDURE p() BEGIN DROP TABLE t; END$$",
                MY,
                ctx(Some(EnvTag::Staging), false, false)
            ),
            (A, None)
        );
    }

    #[test]
    fn production_unbounded_requires_typed_table() {
        let prod = ctx(Some(EnvTag::Production), false, false);
        for (s, d, name) in [
            ("DELETE FROM orders", MY, "orders"),
            ("UPDATE `shop`.`orders` SET a = 1", MY, "shop.orders"),
            (
                "DELETE FROM t1, t2 USING t1 JOIN t2 ON t1.id = t2.id",
                MY,
                "t1",
            ),
            (r#"DELETE FROM "Orders" WHERE 1 = 1"#, PG, "Orders"),
            ("UPDATE t SET a = 1", PG, "t"),
        ] {
            assert_eq!(judged_named(s, d, prod), (C, Some(name.to_string())), "{s}");
        }
        // Elsewhere the setting confirms without a typed name.
        for env in [Some(EnvTag::Staging), Some(EnvTag::Local), None] {
            assert_eq!(
                judged_named("DELETE FROM orders", MY, ctx(env, false, true)),
                (C, None)
            );
        }
        // Plain Explain runs nothing: no confirmation, no name.
        let mut p = plan("DELETE FROM orders", Target::All, MY);
        p.explain = true;
        guard_with(&mut p, prod);
        assert_eq!(
            (p.stmts[0].verdict, p.stmts[0].confirm_name.clone()),
            (A, None)
        );
        // Judged again with another context, the name follows.
        let mut p = plan("DELETE FROM orders", Target::All, MY);
        guard_with(&mut p, prod);
        assert!(p.stmts[0].confirm_name.is_some());
        guard_with(&mut p, ctx(Some(EnvTag::Local), false, true));
        assert_eq!(p.stmts[0].confirm_name, None);
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
