//! Which statements a run executes, with their editor ranges in UTF-16.

use dnt_core::server::SqlKind;
use dnt_core::sql::{
    classify, leading_blank_len, split, Dialect, Statement, StatementKind, TxControl,
};
use dnt_core::tx::{next_autocommit_off, next_tx_state, StmtResult, TxState};
use serde::{Deserialize, Serialize};

use crate::guard::GuardCtx;
use crate::offsets::Utf16Map;
use crate::risk::{drop_targets, DropTarget, RowEstimate};
use crate::run::manual_needs_tx;
use crate::unbounded::{unbounded_writes, UnboundedWrite};

/// What the user asked to run. Offsets are UTF-16 code units into the editor text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub enum Target {
    Cursor(u32),
    Selection(u32, u32),
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Allow,
    Confirm,
    Block,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum StmtKindLabel {
    Read,
    Write,
    Ddl,
    Transaction,
    Other,
    // A Command-family admin or unknown command (`crate::command::label_of`); a plain
    // comment so bindings.ts gains only the `"admin"` literal.
    Admin,
}

impl From<StatementKind> for StmtKindLabel {
    fn from(kind: StatementKind) -> Self {
        match kind {
            StatementKind::Read => Self::Read,
            StatementKind::Write => Self::Write,
            StatementKind::Ddl => Self::Ddl,
            StatementKind::Transaction(_) => Self::Transaction,
            StatementKind::Other => Self::Other,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PlannedStmt {
    pub index: u32,
    pub range: (u32, u32),
    pub text: String,
    pub kind: StatementKind,
    pub verdict: Verdict,
    pub source_range: (usize, usize),
    /// What of this statement changes every row of a table
    /// ([`crate::unbounded::unbounded_writes`]); empty for most statements. Shown by
    /// the confirm dialog; the guard judges the text itself.
    pub unbounded: Vec<UnboundedWrite>,
    /// What of this statement a `DROP` removes ([`crate::risk::drop_targets`]).
    pub drops: Vec<DropTarget>,
    /// Set by the guard on Production for a Confirm statement that changes every row
    /// of a table or drops something ([`crate::risk::confirm_name`]): the run must be
    /// confirmed with this name typed exactly.
    pub confirm_name: Option<String>,
    /// The server's estimate of the rows a flagged `UPDATE` / `DELETE` changes; set
    /// (best-effort) by the backend when it plans on Staging / Production.
    pub estimate: Option<RowEstimate>,
    /// MySQL only: the statement commits the transaction that is open when it runs
    /// (DDL and the other implicit-commit statements), so a later Rollback cannot undo
    /// what ran before it. Set by [`mark_implicit_commits`]; `false` after planning.
    pub implicit_commit: bool,
}

#[derive(Debug, Clone)]
pub struct ScriptPlan {
    pub dialect: Dialect,
    pub source: String,
    pub base: u32,
    pub stmts: Vec<PlannedStmt>,
    pub explain: bool,
    /// Set by [`crate::guard::guard`]: what the verdicts were judged with (`None`:
    /// not guarded, every verdict is `Allow`).
    pub guard: Option<GuardCtx>,
}

/// Plans the statements `target` selects in the editor text `text`.
///
/// - `All` splits the whole text.
/// - `Cursor(c)` splits the whole text and keeps one statement: the one whose
///   range contains `c` (both ends inclusive), else the nearest one ending before
///   `c`, else the first.
/// - `Selection(a, b)` is normalized and clamped, and only that slice is split,
///   starting with the default delimiter (a `DELIMITER` must lie inside it).
///
/// Empty or comment-only input gives an empty plan. Every verdict is `Allow`;
/// the guard sets them.
pub fn plan(text: &str, target: Target, dialect: Dialect) -> ScriptPlan {
    let map = Utf16Map::new(text);
    let (from, to) = match target {
        Target::All | Target::Cursor(_) => (0, text.len()),
        Target::Selection(a, b) => (map.to_byte(a.min(b)), map.to_byte(a.max(b))),
    };
    let source = &text[from..to];
    let mut found = significant(source, dialect);
    let range = |s: &Statement| (map.to_u16(from + s.start), map.to_u16(from + s.end));

    if let Target::Cursor(c) = target {
        // Snap a cursor inside a surrogate pair, or past the end, onto the text.
        let c = map.to_u16(map.to_byte(c));
        let pick = found
            .iter()
            .position(|s| {
                let (start, end) = range(s);
                start <= c && c <= end
            })
            .or_else(|| found.iter().rposition(|s| range(s).1 <= c))
            .or(if found.is_empty() { None } else { Some(0) });
        found = pick.map(|i| found.swap_remove(i)).into_iter().collect();
    }

    let stmts = found
        .into_iter()
        .enumerate()
        .map(|(index, s)| PlannedStmt {
            index: index as u32,
            range: range(&s),
            kind: statement_kind(&s.text, dialect),
            verdict: Verdict::Allow,
            source_range: (s.start, s.end),
            unbounded: unbounded_writes(&s.text, dialect),
            drops: drop_targets(&s.text, dialect),
            confirm_name: None,
            estimate: None,
            implicit_commit: false,
            text: s.text,
        })
        .collect();

    ScriptPlan {
        dialect,
        source: source.to_string(),
        base: map.to_u16(from),
        stmts,
        explain: false,
        guard: None,
    }
}

/// The kind of one planned statement: [`classify`], or, when the statement holds
/// several statements for the server (a MySQL `DELIMITER` left on, see
/// [`crate::guard::resplits`]), the strictest over the statement and its pieces, so
/// the driver's outcome rules (`may_have_committed`, a late result of a `Read`) and the
/// confirm dialog see what really runs:
///
/// - any `Ddl` piece → `Ddl`; else any `Write` → `Write`;
/// - else, with a transaction-control piece: the LAST one (`SELECT 1; BEGIN` is
///   `Transaction(Begin)`), so tx tracking sees the state the text ends in; except
///   that a `ROLLBACK` / `SET autocommit = 0` (which the drivers treat as never
///   committing) next to an `Other` piece gives `Other`, whose effect may be durable;
/// - else `Other` if any piece is, else `Read`.
///
/// Accepted limit: with a `Write`/`Ddl` piece, a `BEGIN`/`COMMIT` piece is not seen by
/// tx tracking (the outcome rules, which matter for "may have been applied", win).
pub fn statement_kind(text: &str, dialect: Dialect) -> StatementKind {
    let whole = classify(text, dialect);
    let pieces = crate::guard::piece_kinds(text, dialect);
    if pieces.is_empty() {
        return whole;
    }
    let all = || std::iter::once(whole).chain(pieces.iter().copied());
    let any = |k: StatementKind| all().any(|p| p == k);
    if any(StatementKind::Ddl) {
        return StatementKind::Ddl;
    }
    if any(StatementKind::Write) {
        return StatementKind::Write;
    }
    let other = any(StatementKind::Other);
    match pieces
        .iter()
        .rev()
        .copied()
        .find(|k| matches!(k, StatementKind::Transaction(_)))
    {
        Some(StatementKind::Transaction(TxControl::Rollback | TxControl::AutocommitOff))
            if other =>
        {
            StatementKind::Other
        }
        Some(tx) => tx,
        None if other => StatementKind::Other,
        None => StatementKind::Read,
    }
}

/// Sets [`PlannedStmt::implicit_commit`] on every statement of `plan` that commits an
/// open transaction, assuming the session is in state `start` when the plan begins
/// and every statement succeeds: on MySQL, a statement that is not transaction
/// control and that `next_tx_state` says ends an open transaction (DDL, `LOCK
/// TABLES`, `FLUSH`, …), when a transaction is open before it. Always `false` on
/// PostgreSQL, whose DDL is transactional. A MySQL `BEGIN` / `START TRANSACTION` run
/// while a transaction is open is flagged too (it commits that transaction first); the
/// user's other transaction control (`COMMIT`, `ROLLBACK`, …) is not: it does what it says.
///
/// `manual_tx`: the plan runs with [`crate::run::RunConfig::manual_tx`], which begins a
/// transaction before every statement that needs one ([`manual_needs_tx`]) while none
/// is open; the walk does the same.
pub fn mark_implicit_commits(plan: &mut ScriptPlan, start: TxState, manual_tx: bool) {
    let dialect = plan.dialect;
    let mysql = dialect.is_mysql();
    let mut state = start;
    let mut autocommit_off = false;
    for s in &mut plan.stmts {
        s.implicit_commit = false;
        if !mysql {
            continue;
        }
        if manual_tx && state == TxState::Idle && manual_needs_tx(s.kind, &s.text, dialect) {
            state = TxState::Active;
        }
        let commits = |prev| {
            next_tx_state(SqlKind::MySql, prev, false, s.kind, &s.text, StmtResult::Ok)
                == TxState::Idle
        };
        // A `BEGIN` in an open transaction commits it before starting the next one.
        let begin = s.kind == StatementKind::Transaction(TxControl::Begin);
        s.implicit_commit = state != TxState::Idle
            && (begin
                || (!matches!(s.kind, StatementKind::Transaction(_)) && commits(TxState::Active)));
        state = next_tx_state(
            SqlKind::MySql,
            state,
            autocommit_off,
            s.kind,
            &s.text,
            StmtResult::Ok,
        );
        autocommit_off =
            next_autocommit_off(SqlKind::MySql, autocommit_off, s.kind, StmtResult::Ok);
    }
}

/// UTF-16 ranges of every statement in `text` (end-exclusive), for highlighting
/// the statement under the cursor. Matches `plan(text, Target::All, dialect)`.
pub fn statement_ranges(text: &str, dialect: Dialect) -> Vec<(u32, u32)> {
    let map = Utf16Map::new(text);
    significant(text, dialect)
        .iter()
        .map(|s| (map.to_u16(s.start), map.to_u16(s.end)))
        .collect()
}

/// `split`, with each statement's leading comments dropped: a comment above a
/// statement is the gap between statements for the cursor rule and the editor
/// highlight, and is not sent to the server.
pub(crate) fn significant(source: &str, dialect: Dialect) -> Vec<Statement> {
    let mut stmts = split(source, dialect);
    for s in &mut stmts {
        let lead = leading_blank_len(&s.text, dialect);
        if lead > 0 {
            s.text.drain(..lead);
            s.start += lead;
        }
    }
    stmts
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    const T: &str = "SELECT 1;\n  -- c\nSELECT 2;\nSELECT 3";
    const MY: Dialect = Dialect::MYSQL;
    const UNI: &str = "SELECT 'Tiếng Việt 😀';\nSELECT 2;";

    fn texts(p: &ScriptPlan) -> Vec<&str> {
        p.stmts.iter().map(|s| s.text.as_str()).collect()
    }

    fn ranges(p: &ScriptPlan) -> Vec<(u32, u32)> {
        p.stmts.iter().map(|s| s.range).collect()
    }

    #[test]
    fn cursor_at_start_of_statement() {
        let p = plan(T, Target::Cursor(17), MY);
        assert_eq!(texts(&p), ["SELECT 2"]);
        assert_eq!(p.stmts[0].index, 0);
        assert_eq!(p.stmts[0].range, (17, 25));
    }

    #[test]
    fn cursor_at_end_of_statement() {
        assert_eq!(texts(&plan(T, Target::Cursor(8), MY)), ["SELECT 1"]);
    }

    #[test]
    fn cursor_in_whitespace_between_takes_previous() {
        assert_eq!(texts(&plan(T, Target::Cursor(10), MY)), ["SELECT 1"]);
    }

    #[test]
    fn cursor_in_comment_between_takes_previous() {
        assert_eq!(texts(&plan(T, Target::Cursor(14), MY)), ["SELECT 1"]);
    }

    #[test]
    fn cursor_before_first_statement_takes_first() {
        let p = plan("  SELECT 1", Target::Cursor(0), MY);
        assert_eq!(texts(&p), ["SELECT 1"]);
        assert_eq!(p.stmts[0].range, (2, 10));
    }

    #[test]
    fn cursor_on_last_line_without_semicolon() {
        let p = plan(T, Target::Cursor(T.len() as u32), MY);
        assert_eq!(texts(&p), ["SELECT 3"]);
    }

    #[test]
    fn cursor_past_the_end_clamps() {
        assert_eq!(texts(&plan(T, Target::Cursor(9999), MY)), ["SELECT 3"]);
    }

    #[test]
    fn empty_text_gives_empty_plan() {
        for text in ["", "  -- x\n"] {
            for target in [
                Target::All,
                Target::Cursor(0),
                Target::Cursor(3),
                Target::Selection(0, 7),
            ] {
                let p = plan(text, target, MY);
                assert!(p.stmts.is_empty(), "{text:?} {target:?}");
            }
        }
    }

    #[test]
    fn all_plans_every_statement() {
        let p = plan(T, Target::All, Dialect::Postgres);
        assert_eq!(texts(&p), ["SELECT 1", "SELECT 2", "SELECT 3"]);
        assert_eq!(
            p.stmts.iter().map(|s| s.index).collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert_eq!(p.source, T);
        assert_eq!(p.base, 0);
        assert_eq!(p.dialect, Dialect::Postgres);
        assert!(!p.explain);
        assert!(p.stmts.iter().all(|s| s.verdict == Verdict::Allow));
        assert!(p.stmts.iter().all(|s| s.kind == StatementKind::Read));
        for s in &p.stmts {
            assert_eq!(&p.source[s.source_range.0..s.source_range.1], s.text);
        }
    }

    #[test]
    fn leading_comments_are_dropped_but_executable_comments_kept() {
        let text = "/* hi */ SELECT 1; /*!40101 SET NAMES utf8 */;";
        let p = plan(text, Target::All, MY);
        assert_eq!(texts(&p), ["SELECT 1", "/*!40101 SET NAMES utf8 */"]);
        assert_eq!(ranges(&p), [(9, 17), (19, 45)]);
        assert_eq!(p.stmts[0].source_range, (9, 17));
    }

    #[test]
    fn selection_splits_only_selected_text() {
        let p = plan(T, Target::Selection(17, 35), MY);
        assert_eq!(texts(&p), ["SELECT 2", "SELECT 3"]);
        assert_eq!(ranges(&p), [(17, 25), (27, 35)]);
        assert_eq!(p.base, 17);
        assert_eq!(p.source, "SELECT 2;\nSELECT 3");
        assert_eq!(p.stmts[0].source_range, (0, 8));
        assert_eq!(p.stmts[1].index, 1);
    }

    #[test]
    fn selection_is_normalized_and_clamped() {
        let p = plan(T, Target::Selection(9999, 17), MY);
        assert_eq!(texts(&p), ["SELECT 2", "SELECT 3"]);
        assert_eq!(p.base, 17);
    }

    #[test]
    fn selection_with_delimiter_block() {
        let block = "DELIMITER $$\nCREATE PROCEDURE p() BEGIN SELECT 1; END$$\nDELIMITER ;";
        let text = format!("SELECT 0;\n{block}\nSELECT 9;");
        let start = 10u32;
        let end = start + block.len() as u32;
        let p = plan(&text, Target::Selection(start, end), MY);
        assert_eq!(texts(&p), ["CREATE PROCEDURE p() BEGIN SELECT 1; END"]);
        assert_eq!(p.stmts[0].kind, StatementKind::Ddl);
    }

    #[test]
    fn utf16_offsets_with_vietnamese_and_emoji() {
        let p = plan(UNI, Target::All, MY);
        assert_eq!(ranges(&p), [(0, 22), (24, 32)]);

        let p = plan(UNI, Target::Cursor(27), MY);
        assert_eq!(texts(&p), ["SELECT 2"]);

        let p = plan(UNI, Target::Selection(24, 32), MY);
        assert_eq!(ranges(&p), [(24, 32)]);
        assert_eq!(p.base, 24);
    }

    #[test]
    fn cursor_inside_surrogate_pair_snaps() {
        let p = plan(UNI, Target::Cursor(20), MY);
        assert_eq!(texts(&p), ["SELECT 'Tiếng Việt 😀'"]);
        assert_eq!(Utf16Map::new(UNI).to_byte(20), UNI.find('😀').unwrap());
    }

    #[test]
    fn selection_edge_inside_surrogate_pair_does_not_panic() {
        // 20 is between the two halves of 😀; the start snaps to the char start.
        let p = plan(UNI, Target::Selection(20, 32), MY);
        assert_eq!(p.base, 19);
        assert_eq!(p.source, "😀';\nSELECT 2");
    }

    #[test]
    fn crlf_offsets() {
        let p = plan("SELECT 1;\r\nSELECT 2", Target::All, MY);
        assert_eq!(ranges(&p), [(0, 8), (11, 19)]);
    }

    #[test]
    fn plan_10k_statements_fast() {
        let text = "INSERT INTO t VALUES (1, 'ư');\n".repeat(10_000);
        let started = Instant::now();
        let p = plan(&text, Target::All, MY);
        let took = started.elapsed();
        assert_eq!(p.stmts.len(), 10_000);
        assert!(took < Duration::from_secs(1), "took {took:?}");
        let last = p.stmts.last().unwrap();
        assert_eq!(last.range, (9_999 * 31, 9_999 * 31 + 29));
        assert_eq!(last.kind, StatementKind::Write);
    }

    #[test]
    fn statement_ranges_match_plan_all() {
        for text in [T, UNI, "SELECT 1;\r\nSELECT 2", "", "  -- x\n"] {
            for dialect in [MY, Dialect::Postgres] {
                assert_eq!(
                    statement_ranges(text, dialect),
                    ranges(&plan(text, Target::All, dialect)),
                    "{text:?}"
                );
            }
        }
    }

    /// A statement holding several statements (a `DELIMITER` left on) takes the
    /// strictest kind of what runs, so the driver's outcome rules see the DELETE.
    #[test]
    fn multi_piece_statement_takes_the_strictest_kind() {
        use dnt_core::sql::TxControl::{AutocommitOff, Begin, Commit};
        use StatementKind::*;
        let p = plan(
            "DELIMITER $$\nSELECT * FROM t; DELETE FROM t WHERE id = 1;",
            Target::All,
            MY,
        );
        assert_eq!(p.stmts.len(), 1);
        assert_eq!(p.stmts[0].kind, Write);
        assert_eq!(StmtKindLabel::from(p.stmts[0].kind), StmtKindLabel::Write);
        for (text, want) in [
            ("SELECT 1; DROP TABLE t", Ddl),
            ("DELETE FROM t; CREATE TABLE u (a int)", Ddl),
            ("SELECT 1; SELECT 2;", Read),
            ("SELECT 1; SET NAMES utf8", Other),
            // Tx tracking sees the transaction the text ends in.
            ("SELECT 1; BEGIN", Transaction(Begin)),
            ("BEGIN; SELECT 1; COMMIT", Transaction(Commit)),
            ("SELECT 1; SET autocommit = 0", Transaction(AutocommitOff)),
            // A rollback next to a possibly durable `Other`: `Other`.
            ("SET GLOBAL x = 1; ROLLBACK", Other),
            // Write wins over transaction control.
            ("BEGIN; DELETE FROM t", Write),
            ("/*!40101 SELECT 1; DELETE FROM t */", Write),
            // Compound bodies keep their own kind; single statements are unchanged.
            ("CREATE PROCEDURE p() BEGIN SELECT 1; END", Ddl),
            ("BEGIN", Transaction(Begin)),
            ("SELECT ';'", Read),
        ] {
            assert_eq!(statement_kind(text, MY), want, "{text}");
        }
        // PG never re-splits.
        assert_eq!(statement_kind("SELECT 1", Dialect::Postgres), Read);
    }

    /// The drivers track the transaction from the planned kind: `SELECT 1; BEGIN`
    /// leaves the session in a transaction.
    #[test]
    fn multi_piece_begin_is_tracked() {
        use dnt_core::server::SqlKind;
        use dnt_core::tx::{next_tx_state, StmtResult, TxState};
        let p = plan("DELIMITER $$\nSELECT 1; BEGIN$$", Target::All, MY);
        let s = &p.stmts[0];
        let after = next_tx_state(
            SqlKind::MySql,
            TxState::Idle,
            false,
            s.kind,
            &s.text,
            StmtResult::Ok,
        );
        assert_eq!(after, TxState::Active);
        let p = plan("DELIMITER $$\nSELECT 1; COMMIT$$", Target::All, MY);
        let s = &p.stmts[0];
        let after = next_tx_state(
            SqlKind::MySql,
            TxState::Active,
            false,
            s.kind,
            &s.text,
            StmtResult::Ok,
        );
        assert_eq!(after, TxState::Idle);
    }

    fn flags(p: &ScriptPlan) -> Vec<bool> {
        p.stmts.iter().map(|s| s.implicit_commit).collect()
    }

    /// MySQL DDL (and the other implicit-commit statements) run while a transaction is
    /// open commit it; PostgreSQL DDL is transactional.
    #[test]
    fn implicit_commits_follow_the_transaction() {
        use dnt_core::tx::TxState::{Active, Idle};
        let text = "CREATE TABLE a (x INT); UPDATE t SET x = 1 WHERE id = 1; \
                    DROP TABLE a; BEGIN; LOCK TABLES t WRITE; COMMIT; ALTER TABLE t ADD y INT";
        let mut p = plan(text, Target::All, MY);
        assert!(flags(&p).iter().all(|f| !f), "planning sets none");
        mark_implicit_commits(&mut p, Idle, false);
        // Idle: the first DDL commits nothing; after BEGIN the LOCK TABLES commits it.
        assert_eq!(flags(&p), [false, false, false, false, true, false, false]);
        mark_implicit_commits(&mut p, Active, false);
        // Active: the CREATE commits it (and leaves the session idle).
        assert_eq!(flags(&p), [true, false, false, false, true, false, false]);
        // Manual: the UPDATE begins a transaction, which the DROP commits.
        mark_implicit_commits(&mut p, Idle, true);
        assert_eq!(flags(&p), [false, false, true, false, true, false, false]);

        let mut p = plan(text, Target::All, Dialect::Postgres);
        mark_implicit_commits(&mut p, Active, true);
        assert!(flags(&p).iter().all(|f| !f));
    }

    /// On MySQL a `BEGIN` / `START TRANSACTION` run while a transaction is open commits
    /// it first: flagged like DDL. PostgreSQL only warns and keeps the transaction.
    #[test]
    fn mysql_begin_in_an_open_transaction_is_an_implicit_commit() {
        use dnt_core::tx::TxState::{Active, Idle};
        let text = "UPDATE t SET x = 1 WHERE id = 1; BEGIN; UPDATE t SET x = 2 WHERE id = 1; \
                    START TRANSACTION; COMMIT; BEGIN";
        let mut p = plan(text, Target::All, MY);
        mark_implicit_commits(&mut p, Active, false);
        assert_eq!(flags(&p), [false, true, false, true, false, false]);
        mark_implicit_commits(&mut p, Idle, false);
        assert_eq!(flags(&p), [false, false, false, true, false, false]);
        let mut p = plan(text, Target::All, Dialect::Postgres);
        mark_implicit_commits(&mut p, Active, false);
        assert!(flags(&p).iter().all(|f| !f));
    }

    #[test]
    fn kind_label_from_statement_kind() {
        use dnt_core::sql::TxControl;
        assert_eq!(
            StmtKindLabel::from(StatementKind::Transaction(TxControl::Commit)),
            StmtKindLabel::Transaction
        );
        assert_eq!(StmtKindLabel::from(StatementKind::Ddl), StmtKindLabel::Ddl);
    }

    #[test]
    fn serde_shapes() {
        assert_eq!(
            serde_json::to_string(&Verdict::Confirm).unwrap(),
            "\"confirm\""
        );
        assert_eq!(
            serde_json::to_string(&StmtKindLabel::Transaction).unwrap(),
            "\"transaction\""
        );
        let t: Target = serde_json::from_str(r#"{"Selection":[1,2]}"#).unwrap();
        assert_eq!(t, Target::Selection(1, 2));
    }
}
