//! Which commands a Command-family run executes, and whether each may run (spec §4.4):
//! the Command counterpart of [`crate::plan::plan`] and [`crate::guard::guard`].

use dnt_core::command::{
    CommandClass, CommandDropKind, CommandLanguage, CommandParseError, CommandSession,
    CommandSessionEffect, ParsedCommand,
};
use dnt_core::driver::{
    ExecOptions, ExecProgress, ResultSummary, RowSink, ServerNotices, StmtOutcome,
};
use dnt_core::error::{AppError, DbError};
use dnt_core::sql::TxControl;
use dnt_core::tx::TxState;
use dnt_core::value::{ColumnMeta, Value};

use crate::offsets::Utf16Map;
use crate::plan::{StmtKindLabel, Target, Verdict};
use crate::policy::{policy, PolicyCtx, RiskFacts};
use crate::run::{
    reconnect_notice, stop_after_error, RunCancel, RunConfig, RunReport, StmtReport, StopReason,
};

/// One planned command.
#[derive(Debug, Clone)]
pub struct PlannedCommand {
    pub index: u32,
    /// UTF-16 range in the editor text.
    pub range: (u32, u32),
    /// Byte range in [`CommandPlan::source`] (as `PlannedStmt::source_range`).
    pub source_range: (usize, usize),
    pub text: String,
    pub parsed: Result<ParsedCommand, CommandParseError>,
    pub label: StmtKindLabel,
    pub verdict: Verdict,
    /// Set by [`guard_commands`] on Production for a Confirm command that needs its
    /// target's (or the connection's) name typed.
    pub confirm_name: Option<String>,
    /// `Some(db)`: the command has db-relative targets or drops, and they were
    /// resolved to `db`; [`run_commands`] does not send it unless the session is in
    /// `db`. `None`: it has none, or the db in effect was unknown.
    pub assumed_db: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CommandPlan {
    /// The text that was split: the whole editor text, or the selection.
    pub source: String,
    /// UTF-16 offset of `source` in the editor text.
    pub base: u32,
    pub stmts: Vec<PlannedCommand>,
    /// The session's database the plan started from (`current_db` of
    /// [`plan_commands`]).
    pub assumed_db: Option<String>,
    /// Set by [`guard_commands`]: what the verdicts were judged with (`None`: not
    /// guarded; only parse errors are `Block`).
    pub guard: Option<PolicyCtx>,
}

impl CommandPlan {
    /// Every command parsed and none is `Block`: the script may run.
    pub fn runnable(&self) -> bool {
        self.stmts
            .iter()
            .all(|s| s.parsed.is_ok() && s.verdict != Verdict::Block)
    }
}

/// The UI label of a command class: `SessionChange` shows as `Other`, and `Admin` and
/// `Unknown` as `Admin`.
pub fn label_of(class: &CommandClass) -> StmtKindLabel {
    match class {
        CommandClass::Read => StmtKindLabel::Read,
        CommandClass::Write => StmtKindLabel::Write,
        CommandClass::Ddl => StmtKindLabel::Ddl,
        CommandClass::Transaction(_) => StmtKindLabel::Transaction,
        CommandClass::SessionChange => StmtKindLabel::Other,
        CommandClass::Admin | CommandClass::Unknown => StmtKindLabel::Admin,
    }
}

/// Plans the commands `target` selects in the editor text `text`, with the same
/// target rules as [`crate::plan::plan`]:
///
/// - `All` splits the whole text.
/// - `Cursor(c)` splits the whole text and keeps one command: the one whose range
///   contains `c` (both ends inclusive), else the nearest one ending before `c`, else
///   the first.
/// - `Selection(a, b)` is normalized and clamped, and only that slice is split.
///
/// `range` (UTF-16) is relative to the editor text; `source_range` (bytes) to the
/// planned slice, `CommandPlan::source`. Empty or whitespace-only input gives an
/// empty plan. A command that does not parse is `Block` (label `Other`); every other
/// verdict is `Allow` until [`guard_commands`] sets them.
///
/// Db-relative targets are resolved to the database in effect when each command
/// runs: `current_db` (the session's) at the first planned command, as changed by
/// every earlier PLANNED command that parses with a `UseDatabase` effect. Commands
/// outside a Cursor or Selection target are not sent, so they change nothing. A
/// target with `database: None` gets that database, and a `Database` drop with an
/// empty name gets it as its name. While it is unknown they stay as they are (the
/// guard then asks for the connection's name). It is unknown when `current_db` is
/// `None` and nothing changed it, and from a `SessionChange` inside a planned
/// transaction (after a `Transaction(Begin)`, before its `Commit` / `Rollback`) to the
/// end of the plan: whether it takes effect depends on how the transaction ends.
pub fn plan_commands(
    text: &str,
    target: Target,
    lang: &dyn CommandLanguage,
    current_db: Option<&str>,
) -> CommandPlan {
    let map = Utf16Map::new(text);
    let (from, to) = match target {
        Target::All | Target::Cursor(_) => (0, text.len()),
        Target::Selection(a, b) => (map.to_byte(a.min(b)), map.to_byte(a.max(b))),
    };
    let source = &text[from..to];
    let range = |r: (usize, usize)| (map.to_u16(from + r.0), map.to_u16(from + r.1));

    let mut found = lang.split(source);

    if let Target::Cursor(c) = target {
        // Snap a cursor inside a surrogate pair, or past the end, onto the text.
        let c = map.to_u16(map.to_byte(c));
        let pick = found
            .iter()
            .position(|s| {
                let (start, end) = range(s.range);
                start <= c && c <= end
            })
            .or_else(|| found.iter().rposition(|s| range(s.range).1 <= c))
            .or(if found.is_empty() { None } else { Some(0) });
        found = pick.map(|i| found.swap_remove(i)).into_iter().collect();
    }

    let mut db = DbTracker {
        db: current_db.map(str::to_string),
        in_tx: false,
        lost: false,
    };
    let stmts = found
        .into_iter()
        .enumerate()
        .map(|(index, s)| {
            let mut parsed = lang.parse(&s.text);
            let mut assumed_db = None;
            if let Ok(cmd) = &mut parsed {
                if resolve_database(cmd, db.db.as_deref()) {
                    assumed_db = db.db.clone();
                }
                db.track(cmd);
            }
            let (label, verdict) = match &parsed {
                Ok(cmd) => (label_of(&cmd.class), Verdict::Allow),
                Err(_) => (StmtKindLabel::Other, Verdict::Block),
            };
            PlannedCommand {
                index: index as u32,
                range: range(s.range),
                source_range: s.range,
                text: s.text,
                parsed,
                label,
                verdict,
                confirm_name: None,
                assumed_db,
            }
        })
        .collect();

    CommandPlan {
        source: source.to_string(),
        base: map.to_u16(from),
        stmts,
        guard: None,
        assumed_db: current_db.map(str::to_string),
    }
}

/// The database in effect while planning (see [`plan_commands`]).
struct DbTracker {
    /// `None`: unknown.
    db: Option<String>,
    /// Inside a planned transaction.
    in_tx: bool,
    /// A `SessionChange` inside a transaction made the db unknown for the rest.
    lost: bool,
}

impl DbTracker {
    /// The database in effect after `cmd`.
    fn track(&mut self, cmd: &ParsedCommand) {
        match &cmd.class {
            CommandClass::Transaction(TxControl::Begin) => self.in_tx = true,
            CommandClass::Transaction(TxControl::Commit | TxControl::Rollback) => {
                self.in_tx = false
            }
            _ => {}
        }
        if let Some(CommandSessionEffect::UseDatabase(name)) = &cmd.session_effect {
            if self.in_tx {
                self.lost = true;
            }
            self.db = (!self.lost).then(|| name.clone());
        }
    }
}

/// Names `db` in `cmd`'s db-relative targets and drops (see [`plan_commands`]).
/// Whether `cmd` has any (named or not: `db` may be `None`).
fn resolve_database(cmd: &mut ParsedCommand, db: Option<&str>) -> bool {
    let mut relative = false;
    for t in &mut cmd.targets {
        if t.database.is_none() {
            relative = true;
            t.database = db.map(str::to_string);
        }
    }
    for d in &mut cmd.drops {
        match d.kind {
            CommandDropKind::Database => {
                if d.target.name.is_empty() {
                    relative = true;
                    d.target.name = db.unwrap_or_default().to_string();
                }
            }
            CommandDropKind::Object => {
                if d.target.database.is_none() {
                    relative = true;
                    d.target.database = db.map(str::to_string);
                }
            }
            CommandDropKind::AllDatabases => {}
        }
    }
    relative
}

/// Sets every command's verdict and `confirm_name` from [`policy`] for the connection
/// `ctx`. A command that does not parse is `Block` with no name (fail-closed).
pub fn guard_commands(plan: &mut CommandPlan, ctx: &PolicyCtx) {
    for s in &mut plan.stmts {
        match &s.parsed {
            Ok(cmd) => {
                let outcome = policy(&RiskFacts::from(cmd), ctx);
                s.verdict = outcome.verdict;
                s.confirm_name = outcome.confirm_name;
            }
            Err(_) => {
                s.verdict = Verdict::Block;
                s.confirm_name = None;
            }
        }
    }
    plan.guard = Some(ctx.clone());
}

/// Where a Command run reports what happens: the Command counterpart of
/// [`crate::run::RunSink`], with the same contract, plus [`CommandRunSink::database`].
pub trait CommandRunSink: RowSink {
    fn statement_start(&mut self, stmt: &PlannedCommand);
    /// `result`: an error is already mapped for the editor (`position` is a UTF-16
    /// offset into the editor text, `tx_state` is the session's state after it).
    fn statement_end(&mut self, stmt: &PlannedCommand, result: &Result<StmtOutcome, AppError>);
    /// A line for Messages about command `index` (after its `statement_end`).
    fn notice(&mut self, index: u32, message: &str);
    /// The session's transaction state changed.
    fn tx_state(&mut self, state: TxState);
    /// A `SessionChange` command succeeded outside a transaction: the session's
    /// database is now `name`. (One sent inside a transaction is not reported: whether
    /// it took effect depends on how the transaction ended.)
    fn database(&mut self, name: &str);
}

/// Runs a guarded command `plan` on `session`, command by command, streaming into
/// `sink` (spec §4.4): the Command counterpart of [`crate::run::run_with`].
///
/// - A plan that was not guarded ([`guard_commands`]) sends nothing: `stop` is
///   `Error { index }` of its first command (0 when it has none).
/// - A plan that is not [`CommandPlan::runnable`] sends nothing: `stop` is
///   `Error { index }` of its first blocked command, `stmts` is empty.
/// - Every `execute_command` gets `opts` with `stop` replaced by `cancel.signal()`.
/// - Before each command the run stops on a cancel (that command is not sent), and
///   before a command with a [`PlannedCommand::assumed_db`] on a session whose
///   `current_database` is another one (`DatabaseChanged`, that command not sent).
/// - After a failure it stops as `run_with` does: on a cancel, a lost connection, an
///   unknown outcome, a command sent inside a transaction or a transaction left open;
///   otherwise only without `continue_on_error`. A failed `UseDatabase` command stops
///   it even with `continue_on_error` (`Error { index }`): the later commands were
///   planned and confirmed against the db it did not select. A late completion stops
///   it too.
/// - A changed `tx_state` goes to `sink.tx_state`; a `SessionChange` that succeeded,
///   sent while the session was `Idle`, to `sink.database`. Reconnects are counted and
///   noticed as in `run_with`.
/// - `cfg.manual_tx` is ignored: the Command family has no manual-commit mode.
pub async fn run_commands(
    session: &dyn CommandSession,
    plan: &CommandPlan,
    opts: &ExecOptions,
    cfg: RunConfig,
    cancel: &RunCancel,
    sink: &mut dyn CommandRunSink,
) -> RunReport {
    let mut report = RunReport {
        stop: None,
        stmts: Vec::new(),
        reconnects: 0,
        began_unused: false,
    };
    if plan.guard.is_none() {
        report.stop = Some(StopReason::Error {
            index: plan.stmts.first().map_or(0, |s| s.index),
        });
        return report;
    }
    if let Some(blocked) = plan
        .stmts
        .iter()
        .find(|s| s.parsed.is_err() || s.verdict == Verdict::Block)
    {
        report.stop = Some(StopReason::Error {
            index: blocked.index,
        });
        return report;
    }
    let opts = ExecOptions {
        stop: cancel.signal(),
        ..opts.clone()
    };
    let mut shown_tx = session.tx_state();
    let mut reconnects = session.reconnects();

    for stmt in &plan.stmts {
        if cancel.is_cancelled() {
            report.stop = Some(StopReason::Cancelled);
            break;
        }
        let Ok(cmd) = &stmt.parsed else {
            // Unreachable: a plan with a command that does not parse is not runnable.
            report.stop = Some(StopReason::Error { index: stmt.index });
            break;
        };
        if let Some(expected) = &stmt.assumed_db {
            let actual = session.current_database();
            if actual.as_ref() != Some(expected) {
                report.stop = Some(StopReason::DatabaseChanged {
                    index: stmt.index,
                    expected: Some(expected.clone()),
                    actual,
                });
                break;
            }
        }
        let before = session.tx_state();
        if before != shown_tx {
            shown_tx = before;
            sink.tx_state(before);
        }

        sink.statement_start(stmt);
        let mut rows = Collect {
            sink: &mut *sink,
            notices: ServerNotices::default(),
        };
        let result = session.execute_command(cmd, &opts, &mut rows).await;
        let server_notices = rows.notices;
        let after = session.tx_state();
        let total = session.reconnects();
        let fresh = total.saturating_sub(reconnects);
        reconnects = total;
        report.reconnects = report
            .reconnects
            .saturating_add(u32::try_from(fresh).unwrap_or(u32::MAX));

        let lost =
            matches!(&result, Err(e) if matches!(e.unstaged(), DbError::ConnectionLost { .. }));
        let (result, stop) = match result {
            Ok(outcome) => {
                let stop = outcome
                    .late_completion
                    .then_some(StopReason::LateCompletion);
                (Ok(outcome), stop)
            }
            Err(err) => {
                let use_db = matches!(
                    cmd.session_effect,
                    Some(CommandSessionEffect::UseDatabase(_))
                );
                let stop = stop_after_error(&err, after, cfg.continue_on_error, stmt.index)
                    .or(use_db.then_some(StopReason::Error { index: stmt.index }));
                let position = command_error_offset(stmt, &err);
                let mut app = AppError::from(err);
                app.tx_state = Some(after.as_str().into());
                app.position = position;
                (Err(app), stop)
            }
        };
        sink.statement_end(stmt, &result);
        for line in server_notices.lines() {
            sink.notice(stmt.index, &line);
        }
        if fresh > 0 && !lost {
            sink.notice(stmt.index, &reconnect_notice(report.reconnects));
        }
        if after != shown_tx {
            shown_tx = after;
            sink.tx_state(after);
        }
        if let (Ok(_), Some(CommandSessionEffect::UseDatabase(name)), TxState::Idle) =
            (&result, &cmd.session_effect, before)
        {
            sink.database(name);
        }
        report.stmts.push(StmtReport {
            index: stmt.index,
            result,
            began_tx: false,
        });
        if stop.is_some() {
            report.stop = stop;
            break;
        }
    }
    report
}

/// The [`RowSink`] a command runs into: results go straight to the run's sink, the
/// server's notices are held until after the command's `statement_end`.
struct Collect<'a> {
    sink: &'a mut dyn CommandRunSink,
    notices: ServerNotices,
}

impl RowSink for Collect<'_> {
    fn result_start(&mut self, cols: &[ColumnMeta]) {
        self.sink.result_start(cols);
    }
    fn rows(&mut self, batch: Vec<Vec<Value>>) {
        self.sink.rows(batch);
    }
    fn result_end(&mut self, summary: &ResultSummary) {
        self.sink.result_end(summary);
    }
    fn progress(&mut self, p: ExecProgress) {
        self.sink.progress(p);
    }
    fn server_notices(&mut self, notices: ServerNotices) {
        self.notices.extend(notices);
    }
}

/// The editor offset (UTF-16) of the error `err` of `stmt`, if the server said where:
/// a `DbError::Server` `position` (1-based chars of `stmt.text`), as
/// [`crate::run::error_offset`] maps it for SQL. `err` is the RAW driver error.
fn command_error_offset(stmt: &PlannedCommand, err: &DbError) -> Option<u32> {
    let DbError::Server {
        position: Some(pos),
        ..
    } = err.unstaged()
    else {
        return None;
    };
    let text = stmt.text.as_str();
    let skip = pos.saturating_sub(1) as usize;
    let byte = text.char_indices().nth(skip).map_or(text.len(), |(b, _)| b);
    Some(stmt.range.0 + Utf16Map::new(text).to_u16(byte))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dnt_core::command::{
        CommandDrop, CommandDropKind, CommandSessionEffect, CommandSpan, CommandTarget,
    };
    use dnt_core::config::EnvTag;
    use dnt_core::redact::Redacted;
    use std::sync::Arc;

    /// One command per line: `GET` is Read, `SET` Write, `USE db` a SessionChange to
    /// `db`, anything else Unknown; a line with `!` does not parse. `FLUSH` drops the
    /// database in effect. Targets name no database.
    pub(super) struct Lines;

    impl CommandLanguage for Lines {
        fn split(&self, text: &str) -> Vec<CommandSpan> {
            let mut out = Vec::new();
            let mut at = 0;
            for line in text.split_inclusive('\n') {
                let lead = line.len() - line.trim_start().len();
                let body = line.trim();
                if !body.is_empty() {
                    let start = at + lead;
                    out.push(CommandSpan {
                        range: (start, start + body.len()),
                        text: body.to_string(),
                    });
                }
                at += line.len();
            }
            out
        }

        fn parse(&self, text: &str) -> Result<ParsedCommand, CommandParseError> {
            if let Some(offset) = text.find('!') {
                return Err(CommandParseError {
                    message: "unexpected '!'".to_string(),
                    offset: Some(offset),
                });
            }
            let mut words = text.split_whitespace();
            let name = words.next().unwrap_or_default().to_string();
            let class = match name.as_str() {
                "GET" => CommandClass::Read,
                "SET" => CommandClass::Write,
                "USE" => CommandClass::SessionChange,
                _ => CommandClass::Unknown,
            };
            let target = words.next().map(str::to_string);
            let session_effect = match (&class, &target) {
                (CommandClass::SessionChange, Some(db)) => {
                    Some(CommandSessionEffect::UseDatabase(db.clone()))
                }
                _ => None,
            };
            let drops = (name == "FLUSH")
                .then(|| CommandDrop {
                    kind: CommandDropKind::Database,
                    target: CommandTarget {
                        database: None,
                        name: String::new(),
                    },
                })
                .into_iter()
                .collect();
            Ok(ParsedCommand {
                name,
                class,
                targets: target
                    .map(|k| CommandTarget {
                        database: None,
                        name: k,
                    })
                    .into_iter()
                    .collect(),
                unbounded: false,
                drops,
                session_effect,
                payload: Arc::new(()),
            })
        }

        fn redact(&self, text: &str) -> Redacted {
            crate::redact::redact(text, dnt_core::sql::Dialect::MYSQL)
        }
    }

    const T: &str = "GET a\nSET b\n\nGET c";

    fn texts(p: &CommandPlan) -> Vec<&str> {
        p.stmts.iter().map(|s| s.text.as_str()).collect()
    }

    fn verdicts(p: &CommandPlan) -> Vec<Verdict> {
        p.stmts.iter().map(|s| s.verdict).collect()
    }

    fn ctx(env: Option<EnvTag>, read_only: bool) -> PolicyCtx {
        PolicyCtx {
            env,
            read_only,
            confirm_unbounded: false,
            connection_name: "conn".to_string(),
        }
    }

    fn guarded(text: &str, ctx: &PolicyCtx) -> CommandPlan {
        let mut p = plan_commands(text, Target::All, &Lines, None);
        guard_commands(&mut p, ctx);
        p
    }

    #[test]
    fn cursor_picks_the_line() {
        for (cursor, want) in [
            (0, "GET a"),
            (5, "GET a"),
            (6, "SET b"),
            (11, "SET b"),
            // The blank line between: the nearest command before it.
            (12, "SET b"),
            (13, "GET c"),
            (9999, "GET c"),
        ] {
            let p = plan_commands(T, Target::Cursor(cursor), &Lines, None);
            assert_eq!(texts(&p), [want], "cursor {cursor}");
            assert_eq!(p.stmts[0].index, 0);
            assert_eq!(p.base, 0);
        }
        let p = plan_commands("  GET a", Target::Cursor(0), &Lines, None);
        assert_eq!(texts(&p), ["GET a"]);
        assert_eq!(p.stmts[0].range, (2, 7));
    }

    #[test]
    fn selection_plans_only_the_slice() {
        let p = plan_commands(T, Target::Selection(18, 6), &Lines, None);
        assert_eq!(texts(&p), ["SET b", "GET c"]);
        assert_eq!(p.source, "SET b\n\nGET c");
        assert_eq!(p.base, 6);
        let ranges: Vec<_> = p.stmts.iter().map(|s| s.range).collect();
        assert_eq!(ranges, [(6, 11), (13, 18)]);
        for s in &p.stmts {
            assert_eq!(&p.source[s.source_range.0..s.source_range.1], s.text);
        }
        assert_eq!(p.stmts[0].source_range, (0, 5));
        assert_eq!(p.stmts[1].source_range, (7, 12));
        assert_eq!(p.stmts[1].index, 1);
        assert_eq!(p.stmts[0].label, StmtKindLabel::Write);
        assert_eq!(p.stmts[1].label, StmtKindLabel::Read);
    }

    #[test]
    fn empty_input_gives_empty_plan() {
        for text in ["", "  \n\t\n"] {
            for target in [
                Target::All,
                Target::Cursor(0),
                Target::Cursor(3),
                Target::Selection(0, 4),
            ] {
                let p = plan_commands(text, target, &Lines, None);
                assert!(p.stmts.is_empty(), "{text:?} {target:?}");
                assert!(p.runnable());
            }
        }
    }

    #[test]
    fn ranges_are_utf16() {
        let text = "SET k \"héllo😀\"\nGET k";
        let get = text.find("GET").unwrap();
        let get16 = text[..get].encode_utf16().count() as u32;
        assert_eq!((get, get16), (19, 16));
        let p = plan_commands(text, Target::All, &Lines, None);
        assert_eq!(p.stmts[0].range, (0, 15));
        assert_eq!(p.stmts[1].range, (get16, get16 + 5));
        assert_eq!(p.stmts[1].source_range, (get, get + 5));
        let p = plan_commands(text, Target::Cursor(17), &Lines, None);
        assert_eq!(texts(&p), ["GET k"]);
        let p = plan_commands(text, Target::Selection(16, 21), &Lines, None);
        assert_eq!(texts(&p), ["GET k"]);
        assert_eq!(p.base, 16);
        assert_eq!(p.stmts[0].range, (16, 21));
        // Bytes in the slice `p.source` ("GET k"), not in the editor text.
        assert_eq!(p.source, "GET k");
        assert_eq!(p.stmts[0].source_range, (0, 5));
    }

    #[test]
    fn parse_error_blocks_the_whole_script() {
        let text = "GET a\nSET !\nGET b";
        let p = plan_commands(text, Target::All, &Lines, None);
        assert_eq!(
            verdicts(&p)[1],
            Verdict::Block,
            "fail-closed before the guard"
        );
        assert!(!p.runnable());

        let p = guarded(text, &ctx(Some(EnvTag::Local), false));
        assert_eq!(
            verdicts(&p),
            [Verdict::Allow, Verdict::Block, Verdict::Allow]
        );
        let bad = &p.stmts[1];
        assert_eq!(bad.label, StmtKindLabel::Other);
        assert_eq!(bad.confirm_name, None);
        assert_eq!(bad.parsed.as_ref().unwrap_err().offset, Some(4));
        assert!(!p.runnable());
        assert_eq!(p.guard, Some(ctx(Some(EnvTag::Local), false)));
    }

    /// The database each command's targets and drops resolve to.
    fn resolved(p: &CommandPlan) -> Vec<(Vec<Option<&str>>, Vec<&str>)> {
        p.stmts
            .iter()
            .map(|s| {
                let cmd = s.parsed.as_ref().unwrap();
                (
                    cmd.targets.iter().map(|t| t.database.as_deref()).collect(),
                    cmd.drops.iter().map(|d| d.target.name.as_str()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn use_database_flows_into_later_targets() {
        let text = "NUKE k\nUSE a\nNUKE k\nFLUSH";
        let p = plan_commands(text, Target::All, &Lines, Some("x"));
        assert_eq!(
            resolved(&p),
            [
                (vec![Some("x")], vec![]),
                // `USE a`'s own target is resolved before its effect applies.
                (vec![Some("x")], vec![]),
                (vec![Some("a")], vec![]),
                (vec![], vec!["a"]),
            ]
        );

        // A `USE` outside a Cursor or Selection plan is not sent: it changes nothing.
        let p = plan_commands("USE a\nNUKE k", Target::Cursor(7), &Lines, Some("x"));
        assert_eq!(texts(&p), ["NUKE k"]);
        assert_eq!(resolved(&p), [(vec![Some("x")], vec![])]);
        let p = plan_commands("USE a\nFLUSH", Target::Selection(6, 11), &Lines, Some("x"));
        assert_eq!(texts(&p), ["FLUSH"]);
        assert_eq!(resolved(&p), [(vec![], vec!["x"])]);
        // One inside the selection does.
        let p = plan_commands(
            "GET z\nUSE a\nFLUSH",
            Target::Selection(6, 17),
            &Lines,
            Some("x"),
        );
        assert_eq!(texts(&p), ["USE a", "FLUSH"]);
        assert_eq!(
            resolved(&p),
            [(vec![Some("x")], vec![]), (vec![], vec!["a"])]
        );

        // A `USE` that does not parse changes nothing; an unknown db stays unknown.
        let p = plan_commands("USE b!\nNUKE k", Target::All, &Lines, Some("x"));
        assert_eq!(
            p.stmts[1].parsed.as_ref().unwrap().targets[0]
                .database
                .as_deref(),
            Some("x")
        );
        let p = plan_commands("NUKE k\nFLUSH", Target::All, &Lines, None);
        assert_eq!(resolved(&p), [(vec![None], vec![]), (vec![], vec![""])]);
    }

    #[test]
    fn unknown_on_production_needs_connection_name() {
        let p = guarded(
            "NUKE\nNUKE k1\nGET a",
            &ctx(Some(EnvTag::Production), false),
        );
        assert_eq!(
            verdicts(&p),
            [Verdict::Confirm, Verdict::Confirm, Verdict::Allow]
        );
        assert_eq!(p.stmts[0].confirm_name.as_deref(), Some("conn"));
        assert_eq!(p.stmts[1].confirm_name.as_deref(), Some("k1"));
        assert_eq!(p.stmts[2].confirm_name, None);
        assert_eq!(p.stmts[0].label, StmtKindLabel::Admin);
        assert!(p.runnable());

        let p = guarded("NUKE", &ctx(Some(EnvTag::Staging), false));
        assert_eq!(verdicts(&p), [Verdict::Allow]);
        assert_eq!(p.stmts[0].confirm_name, None);
    }

    #[test]
    fn read_only_blocks_writes() {
        for env in [Some(EnvTag::Production), Some(EnvTag::Local), None] {
            let p = guarded("GET a\nSET b\nNUKE", &ctx(env, true));
            assert_eq!(
                verdicts(&p),
                [Verdict::Allow, Verdict::Block, Verdict::Block],
                "{env:?}"
            );
            assert!(p.stmts.iter().all(|s| s.confirm_name.is_none()));
            assert!(!p.runnable());
        }
        assert!(guarded("GET a", &ctx(None, true)).runnable());
    }

    #[test]
    fn label_of_maps_classes() {
        use dnt_core::sql::TxControl;
        for (class, want) in [
            (CommandClass::Read, StmtKindLabel::Read),
            (CommandClass::Write, StmtKindLabel::Write),
            (CommandClass::Ddl, StmtKindLabel::Ddl),
            (
                CommandClass::Transaction(TxControl::Begin),
                StmtKindLabel::Transaction,
            ),
            (CommandClass::SessionChange, StmtKindLabel::Other),
            (CommandClass::Admin, StmtKindLabel::Admin),
            (CommandClass::Unknown, StmtKindLabel::Admin),
        ] {
            assert_eq!(label_of(&class), want, "{class:?}");
        }
        assert_eq!(
            serde_json::to_string(&StmtKindLabel::Admin).unwrap(),
            "\"admin\""
        );
    }
}

/// [`run_commands`] against a scripted fake [`CommandSession`] (named so that
/// `cargo test -p dnt-query command::run` selects it).
#[cfg(test)]
mod run_tests {
    use super::tests::Lines;
    use super::*;
    use crate::run::{reconnect_notice, RunCancel, RunConfig, RunReport, StopReason};
    use async_trait::async_trait;
    use dnt_core::config::EnvTag;
    use dnt_core::data_access::{DataAccess, KeyEdit, PageSpec};
    use dnt_core::driver::{
        CancelHandle, Canceller, Engine, ExecOptions, Introspector, ResultSummary, RowSink,
        Session, StmtOutcome,
    };
    use dnt_core::edit::EditSet;
    use dnt_core::error::{AppError, ConnectStage, DbError};
    use dnt_core::meta::{
        DatabaseInfo, ObjectKind, ObjectRef, ObjectSummary, SchemaInfo, Scope, TableColumns,
        TableDetail,
    };
    use dnt_core::server::{detect_pg, ServerInfo};
    use dnt_core::sql::filter::DataFilter;
    use dnt_core::tx::TxState;
    use dnt_core::value::{ColumnMeta, Value};
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    /// What the fake does for one `execute_command` call.
    struct Step {
        result: Result<StmtOutcome, DbError>,
        /// The session's transaction state afterwards (`None`: unchanged).
        tx_after: Option<TxState>,
        /// Reconnects the driver made during the call.
        reconnects: u64,
    }

    fn ok() -> Step {
        Step {
            result: Ok(StmtOutcome::default()),
            tx_after: None,
            reconnects: 0,
        }
    }

    fn fail(err: DbError) -> Step {
        Step {
            result: Err(err),
            ..ok()
        }
    }

    impl Step {
        fn tx(mut self, state: TxState) -> Self {
            self.tx_after = Some(state);
            self
        }
        fn reconnects(mut self, n: u64) -> Self {
            self.reconnects = n;
            self
        }
    }

    fn server_err(position: Option<u32>) -> DbError {
        DbError::Server {
            code: None,
            sqlstate: None,
            message: "bad command".into(),
            position,
        }
    }

    fn lost() -> DbError {
        DbError::ConnectionLost {
            was_in_tx: false,
            outcome_unknown: false,
            message: "gone".into(),
        }
    }

    struct NoCancel;

    #[async_trait]
    impl Canceller for NoCancel {
        async fn cancel(&self) -> Result<(), DbError> {
            Ok(())
        }
    }

    struct FakeCmd {
        info: ServerInfo,
        steps: Mutex<VecDeque<Step>>,
        /// `name target` of every command received.
        received: Mutex<Vec<String>>,
        /// Whether `opts.stop` was the run's own signal, per command received.
        stop_is_run_signal: Mutex<Vec<bool>>,
        run_signal: Mutex<Option<dnt_core::driver::StopSignal>>,
        tx: Mutex<TxState>,
        reconnects: AtomicU64,
        /// The current db: set by a successful `USE`.
        db: Mutex<Option<String>>,
    }

    impl FakeCmd {
        fn new(steps: Vec<Step>) -> Self {
            FakeCmd {
                info: detect_pg(170002, "PostgreSQL 17.2"),
                steps: Mutex::new(steps.into()),
                received: Mutex::new(vec![]),
                stop_is_run_signal: Mutex::new(vec![]),
                run_signal: Mutex::new(None),
                tx: Mutex::new(TxState::Idle),
                reconnects: AtomicU64::new(0),
                db: Mutex::new(None),
            }
        }

        fn received(&self) -> Vec<String> {
            self.received.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Session for FakeCmd {
        fn server_info(&self) -> &ServerInfo {
            &self.info
        }
        fn engine(&self) -> Engine<'_> {
            Engine::Command(self)
        }
        fn introspector(&self) -> &dyn Introspector {
            self
        }
        fn cancel_handle(&self) -> CancelHandle {
            Arc::new(NoCancel)
        }
        fn reconnects(&self) -> u64 {
            self.reconnects.load(Ordering::SeqCst)
        }
        async fn ping(&self) -> Result<(), DbError> {
            Ok(())
        }
        async fn close(self: Box<Self>) {}
    }

    #[async_trait]
    impl CommandSession for FakeCmd {
        fn language(&self) -> &dyn CommandLanguage {
            &Lines
        }
        fn tx_state(&self) -> TxState {
            *self.tx.lock().unwrap()
        }
        fn current_database(&self) -> Option<String> {
            self.db.lock().unwrap().clone()
        }
        fn data_access(&self) -> &dyn DataAccess {
            self
        }
        async fn execute_command(
            &self,
            cmd: &ParsedCommand,
            opts: &ExecOptions,
            sink: &mut dyn RowSink,
        ) -> Result<StmtOutcome, DbError> {
            let target = cmd.targets.first().map_or("", |t| t.name.as_str());
            self.received
                .lock()
                .unwrap()
                .push(format!("{} {target}", cmd.name).trim_end().to_string());
            if let Some(run) = &*self.run_signal.lock().unwrap() {
                // Raising the run's signal must show on `opts.stop`.
                let same = !opts.stop.is_stopped() && {
                    run.stop();
                    opts.stop.is_stopped()
                };
                self.stop_is_run_signal.lock().unwrap().push(same);
            }
            if opts.stop.is_stopped() {
                return Err(DbError::cancelled());
            }
            let step = self.steps.lock().unwrap().pop_front().unwrap_or_else(ok);
            if let Some(state) = step.tx_after {
                *self.tx.lock().unwrap() = state;
            }
            self.reconnects.fetch_add(step.reconnects, Ordering::SeqCst);
            if let (Ok(_), Some(CommandSessionEffect::UseDatabase(db))) =
                (&step.result, &cmd.session_effect)
            {
                *self.db.lock().unwrap() = Some(db.clone());
            }
            if let Ok(outcome) = &step.result {
                for r in &outcome.results {
                    sink.result_start(&[]);
                    sink.result_end(r);
                }
            }
            step.result
        }
    }

    fn n_a() -> DbError {
        DbError::Internal {
            message: "n/a".into(),
        }
    }

    impl DataAccess for FakeCmd {
        fn page(&self, _obj: &ObjectRef, _spec: &PageSpec) -> Result<ParsedCommand, DbError> {
            Err(n_a())
        }
        fn count(
            &self,
            _obj: &ObjectRef,
            _filter: Option<&DataFilter>,
        ) -> Result<ParsedCommand, DbError> {
            Err(n_a())
        }
        fn build_edits(
            &self,
            _obj: &ObjectRef,
            _detail: &TableDetail,
            _edits: &EditSet,
        ) -> Result<Vec<ParsedCommand>, DbError> {
            Err(n_a())
        }
        fn build_key_edit(
            &self,
            _db: &str,
            _edit: &KeyEdit,
        ) -> Result<Vec<ParsedCommand>, DbError> {
            Err(n_a())
        }
    }

    #[async_trait]
    impl Introspector for FakeCmd {
        async fn list_databases(&self) -> Result<Vec<DatabaseInfo>, DbError> {
            Ok(vec![])
        }
        async fn list_schemas(&self, _db: &str) -> Result<Vec<SchemaInfo>, DbError> {
            Ok(vec![])
        }
        async fn list_objects(
            &self,
            _scope: &Scope,
            _kind: ObjectKind,
        ) -> Result<Vec<ObjectSummary>, DbError> {
            Ok(vec![])
        }
        async fn list_columns(&self, _scope: &Scope) -> Result<Vec<TableColumns>, DbError> {
            Ok(vec![])
        }
        async fn table_detail(&self, _obj: &ObjectRef) -> Result<TableDetail, DbError> {
            Err(n_a())
        }
        async fn object_ddl(&self, _obj: &ObjectRef) -> Result<String, DbError> {
            Ok(String::new())
        }
    }

    #[derive(Default)]
    struct Rec {
        events: Vec<String>,
        notices: Vec<(u32, String)>,
        tx: Vec<TxState>,
        databases: Vec<String>,
        /// Raised inside `statement_end` of this command.
        cancel_at_end: Option<(u32, RunCancel)>,
    }

    impl RowSink for Rec {
        fn result_start(&mut self, cols: &[ColumnMeta]) {
            self.events.push(format!("result_start {}", cols.len()));
        }
        fn rows(&mut self, batch: Vec<Vec<Value>>) {
            self.events.push(format!("rows {}", batch.len()));
        }
        fn result_end(&mut self, summary: &ResultSummary) {
            self.events
                .push(format!("result_end {}", summary.row_count));
        }
    }

    impl CommandRunSink for Rec {
        fn statement_start(&mut self, stmt: &PlannedCommand) {
            self.events.push(format!("start {}", stmt.index));
        }
        fn statement_end(&mut self, stmt: &PlannedCommand, result: &Result<StmtOutcome, AppError>) {
            self.events.push(format!(
                "end {} {}",
                stmt.index,
                if result.is_ok() { "ok" } else { "err" }
            ));
            if let Some((index, cancel)) = &self.cancel_at_end {
                if *index == stmt.index {
                    cancel.signal().stop();
                }
            }
        }
        fn notice(&mut self, index: u32, message: &str) {
            self.notices.push((index, message.to_string()));
        }
        fn tx_state(&mut self, state: TxState) {
            self.tx.push(state);
        }
        fn database(&mut self, name: &str) {
            self.databases.push(name.to_string());
        }
    }

    fn cfg(continue_on_error: bool) -> RunConfig {
        RunConfig {
            continue_on_error,
            manual_tx: false,
        }
    }

    fn cancel() -> RunCancel {
        RunCancel::new(Arc::new(NoCancel))
    }

    async fn go(
        session: &FakeCmd,
        text: &str,
        cfg: RunConfig,
        cancel: &RunCancel,
        rec: &mut Rec,
    ) -> RunReport {
        let mut plan = plan_commands(text, Target::All, &Lines, None);
        guard_commands(&mut plan, &local());
        run_commands(session, &plan, &ExecOptions::default(), cfg, cancel, rec).await
    }

    fn local() -> PolicyCtx {
        PolicyCtx {
            env: Some(EnvTag::Local),
            read_only: false,
            confirm_unbounded: false,
            connection_name: "conn".into(),
        }
    }

    #[tokio::test]
    async fn blocked_plan_sends_nothing() {
        // A parse error blocks the script before any guard.
        let s = FakeCmd::new(vec![]);
        let mut rec = Rec::default();
        let report = go(&s, "GET a\nSET !\nGET b", cfg(true), &cancel(), &mut rec).await;
        assert_eq!(report.stop, Some(StopReason::Error { index: 1 }));
        assert!(report.stmts.is_empty());
        assert_eq!(report.reconnects, 0);
        assert!(s.received().is_empty());
        assert!(rec.events.is_empty() && rec.notices.is_empty() && rec.tx.is_empty());

        // A write on a read-only connection.
        let mut plan = plan_commands("GET a\nGET b\nSET c\nSET d", Target::All, &Lines, None);
        guard_commands(
            &mut plan,
            &PolicyCtx {
                env: Some(EnvTag::Local),
                read_only: true,
                confirm_unbounded: false,
                connection_name: "conn".into(),
            },
        );
        let mut rec = Rec::default();
        let report = run_commands(
            &s,
            &plan,
            &ExecOptions::default(),
            cfg(false),
            &cancel(),
            &mut rec,
        )
        .await;
        assert_eq!(report.stop, Some(StopReason::Error { index: 2 }));
        assert!(report.stmts.is_empty());
        assert!(s.received().is_empty());
        assert!(rec.events.is_empty());
    }

    #[tokio::test]
    async fn unguarded_plan_sends_nothing() {
        let s = FakeCmd::new(vec![]);
        let plan = plan_commands("GET a\nSET b", Target::All, &Lines, None);
        assert!(plan.guard.is_none() && plan.runnable());
        let mut rec = Rec::default();
        let report = run_commands(
            &s,
            &plan,
            &ExecOptions::default(),
            cfg(true),
            &cancel(),
            &mut rec,
        )
        .await;
        assert_eq!(report.stop, Some(StopReason::Error { index: 0 }));
        assert!(report.stmts.is_empty());
        assert!(s.received().is_empty());
        assert!(rec.events.is_empty());
    }

    #[tokio::test]
    async fn stops_at_first_error() {
        let s = FakeCmd::new(vec![ok(), fail(server_err(Some(3)))]);
        let mut rec = Rec::default();
        let text = "GET a\n  BAD xy\nGET c";
        let report = go(&s, text, cfg(false), &cancel(), &mut rec).await;
        assert_eq!(s.received(), ["GET a", "BAD xy"]);
        assert_eq!(report.stop, Some(StopReason::Error { index: 1 }));
        assert_eq!(report.stmts.len(), 2);
        assert!(report.stmts[0].result.is_ok());
        assert!(report.stmts.iter().all(|s| !s.began_tx));
        let err = report.stmts[1].result.as_ref().unwrap_err();
        // 1-based char 3 of "BAD xy", which starts at UTF-16 offset 8.
        assert_eq!(err.position, Some(10));
        assert_eq!(err.tx_state.as_deref(), Some("idle"));
        assert_eq!(rec.events, ["start 0", "end 0 ok", "start 1", "end 1 err"]);

        // No offset from the server: no position.
        let s = FakeCmd::new(vec![fail(server_err(None))]);
        let report = go(&s, "BAD", cfg(false), &cancel(), &mut Rec::default()).await;
        assert_eq!(report.stmts[0].result.as_ref().unwrap_err().position, None);
    }

    #[tokio::test]
    async fn continue_on_error_goes_on() {
        let s = FakeCmd::new(vec![ok(), fail(server_err(None)), ok()]);
        let mut rec = Rec::default();
        let report = go(&s, "GET a\nBAD b\nGET c", cfg(true), &cancel(), &mut rec).await;
        assert_eq!(s.received(), ["GET a", "BAD b", "GET c"]);
        assert_eq!(report.stop, None);
        assert_eq!(report.stmts.len(), 3);
        assert!(report.stmts[1].result.is_err());

        // A lost connection or an unknown outcome stops it anyway.
        let unknown = DbError::Timeout {
            stage: ConnectStage::Execute,
            outcome_unknown: true,
            was_in_tx: false,
        };
        for err in [lost(), unknown] {
            let s = FakeCmd::new(vec![ok(), fail(err)]);
            let report = go(
                &s,
                "GET a\nSET b\nGET c",
                cfg(true),
                &cancel(),
                &mut Rec::default(),
            )
            .await;
            assert_eq!(s.received(), ["GET a", "SET b"]);
            assert_eq!(report.stop, Some(StopReason::Error { index: 1 }));
        }
    }

    #[tokio::test]
    async fn cancel_before_second_stops() {
        let s = FakeCmd::new(vec![]);
        let cancel = cancel();
        let mut rec = Rec {
            cancel_at_end: Some((0, cancel.clone())),
            ..Rec::default()
        };
        let report = go(&s, "GET a\nGET b\nGET c", cfg(true), &cancel, &mut rec).await;
        assert_eq!(s.received(), ["GET a"]);
        assert_eq!(report.stop, Some(StopReason::Cancelled));
        assert_eq!(report.stmts.len(), 1);
        assert_eq!(rec.events, ["start 0", "end 0 ok"]);
    }

    #[tokio::test]
    async fn every_command_gets_the_run_signal() {
        let s = FakeCmd::new(vec![]);
        let cancel = cancel();
        *s.run_signal.lock().unwrap() = Some(cancel.signal());
        let report = go(&s, "GET a\nGET b", cfg(false), &cancel, &mut Rec::default()).await;
        assert_eq!(*s.stop_is_run_signal.lock().unwrap(), [true]);
        // The fake raised the run's signal inside the first command, so the second is
        // never sent.
        assert_eq!(s.received(), ["GET a"]);
        assert_eq!(report.stop, Some(StopReason::Cancelled));
    }

    #[tokio::test]
    async fn tx_state_changes_are_reported() {
        let s = FakeCmd::new(vec![ok().tx(TxState::Active), ok(), ok().tx(TxState::Idle)]);
        let mut rec = Rec::default();
        let report = go(&s, "BEGIN\nSET a\nCOMMIT", cfg(false), &cancel(), &mut rec).await;
        assert_eq!(report.stop, None);
        assert_eq!(rec.tx, [TxState::Active, TxState::Idle]);

        // A failed command that leaves a transaction open stops the run, and its
        // error carries the state.
        let s = FakeCmd::new(vec![ok().tx(TxState::Active), fail(server_err(None))]);
        let mut rec = Rec::default();
        let report = go(&s, "BEGIN\nSET a\nGET b", cfg(true), &cancel(), &mut rec).await;
        assert_eq!(report.stop, Some(StopReason::Error { index: 1 }));
        assert_eq!(rec.tx, [TxState::Active]);
        let err = report.stmts[1].result.as_ref().unwrap_err();
        assert_eq!(err.tx_state.as_deref(), Some("active"));
    }

    #[tokio::test]
    async fn successful_use_reports_the_database() {
        // A failed `USE` is not reported (and stops the run, see below).
        let s = FakeCmd::new(vec![ok(), ok(), fail(server_err(None))]);
        let mut rec = Rec::default();
        let report = go(
            &s,
            "USE one\nGET a\nUSE two",
            cfg(true),
            &cancel(),
            &mut rec,
        )
        .await;
        assert_eq!(report.stop, Some(StopReason::Error { index: 2 }));
        assert_eq!(rec.databases, ["one"]);
    }

    #[tokio::test]
    async fn failed_use_stops_even_with_continue_on_error() {
        // Later commands were planned (and confirmed) against the db it failed to select.
        let s = FakeCmd::new(vec![fail(server_err(None))]);
        let mut rec = Rec::default();
        let report = go(&s, "USE bad\nSET x", cfg(true), &cancel(), &mut rec).await;
        assert_eq!(s.received(), ["USE bad"]);
        assert_eq!(report.stop, Some(StopReason::Error { index: 0 }));
        assert_eq!(report.stmts.len(), 1);
        assert!(rec.databases.is_empty());
    }

    #[tokio::test]
    async fn reconnects_are_counted_and_noticed() {
        let s = FakeCmd::new(vec![ok().reconnects(1), ok(), fail(lost()).reconnects(1)]);
        let mut rec = Rec::default();
        let report = go(&s, "GET a\nGET b\nGET c", cfg(true), &cancel(), &mut rec).await;
        assert_eq!(report.reconnects, 2);
        assert_eq!(report.stop, Some(StopReason::Error { index: 2 }));
        // The lost connection shows in its own error, not as a notice.
        assert_eq!(rec.notices, [(0, reconnect_notice(1))]);
    }
}
