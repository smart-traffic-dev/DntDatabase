//! Runs a planned script on a session, one statement at a time, and decides when
//! the script stops (spec §4.2, §4.5, §4.6, §9).
//!
//! A run never re-splits what it sends: every statement goes to the server as the
//! plan holds it (`PlannedStmt::text`), so a one-statement plan (Retry, Fetch all)
//! runs exactly that statement.

use dnt_core::driver::{CancelHandle, ExecOptions, RowSink, Session, StmtOutcome, StopSignal};
use dnt_core::error::{AppError, DbError};
use dnt_core::sql::{classify, Dialect, StatementKind, TxControl};
use dnt_core::tx::{is_rollback_to, TxState};
use serde::{Deserialize, Serialize};

use crate::guard::stmt_verdict;
use crate::offsets::Utf16Map;
use crate::plan::{significant, PlannedStmt, ScriptPlan};

/// Where a run reports what happens. Results stream through the [`RowSink`] part,
/// between `statement_start` and `statement_end` of their statement.
pub trait RunSink: RowSink {
    fn statement_start(&mut self, stmt: &PlannedStmt);
    /// `result`: an error is already mapped for the editor (`position` is a UTF-16
    /// offset into the editor text, `tx_state` is the session's state after it).
    fn statement_end(&mut self, stmt: &PlannedStmt, result: &Result<StmtOutcome, AppError>);
    /// A line for Messages about statement `index` (after its `statement_end`).
    fn notice(&mut self, index: u32, message: &str);
    /// The session's transaction state changed.
    fn tx_state(&mut self, state: TxState);
}

/// Stops one run: the run's own [`StopSignal`] (which `run` hands to every
/// `execute_one`, so no statement is sent once it is raised) and the session's
/// [`CancelHandle`] (which stops the statement that is running). Build one per run:
/// the signal stays raised.
#[derive(Clone)]
pub struct RunCancel {
    stop: StopSignal,
    handle: CancelHandle,
}

impl RunCancel {
    pub fn new(handle: CancelHandle) -> Self {
        RunCancel {
            stop: StopSignal::new(),
            handle,
        }
    }

    pub fn signal(&self) -> StopSignal {
        self.stop.clone()
    }

    pub fn is_cancelled(&self) -> bool {
        self.stop.is_stopped()
    }

    /// Raises the stop signal, THEN cancels the running statement: a statement that
    /// is about to start sees the signal and is never sent.
    pub async fn cancel(&self) -> Result<(), DbError> {
        self.stop.stop();
        self.handle.cancel().await
    }
}

/// Why a run ended before its last statement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StopReason {
    /// Statement `index` failed.
    Error { index: u32 },
    /// The user stopped the run.
    Cancelled,
    /// The PostgreSQL transaction is aborted: only `ROLLBACK [TO]` / `COMMIT` may run.
    TxAborted,
    /// A statement finished after its timeout; the rest was not run.
    LateCompletion,
    /// The session's `sql_mode` now splits the rest differently, starting at statement
    /// `index`: "sql_mode changed; run the remaining statements again".
    DialectChanged { index: u32 },
}

/// One statement that was handed to the session.
#[derive(Debug, Clone)]
pub struct StmtReport {
    pub index: u32,
    pub result: Result<StmtOutcome, AppError>,
}

#[derive(Debug, Clone)]
pub struct RunReport {
    /// `None`: every planned statement ran.
    ///
    /// For the UI: a cancel only shows here when it stopped something. A cancel that
    /// lands while the LAST statement runs, when that statement still completes `Ok`,
    /// gives `None` (everything ran); `RunCancel::is_cancelled` still says the user
    /// pressed Stop.
    pub stop: Option<StopReason>,
    /// The statements sent, in order.
    pub stmts: Vec<StmtReport>,
    /// Connections the session replaced during the run.
    pub reconnects: u32,
}

/// Messages line after a `COMMIT` that ended an aborted transaction.
pub const ROLLED_BACK_NOTICE: &str = "Transaction was rolled back";

/// Messages line for a reconnect the statement's result does not show (it succeeded,
/// or failed for another reason, on a fresh connection).
pub fn reconnect_notice(total: u32) -> String {
    if total == 1 {
        "Reconnected to the server; session state was reset".into()
    } else {
        format!(
            "Reconnected to the server; session state was reset ({total} reconnects in this run)"
        )
    }
}

/// Runs `plan` on `session`, statement by statement, streaming into `sink`.
///
/// Every `execute_one` gets `opts` with `stop` replaced by `cancel.signal()`.
/// Before each statement the run stops on a cancel, on a `sql_mode` change that
/// splits the rest differently or changes a guard verdict of the rest, and on an
/// aborted transaction (anything but
/// `ROLLBACK [TO]` / `COMMIT` / `END` / `ABORT`). After a failure it stops on a
/// cancel, a lost connection, an unknown outcome, a statement sent inside a
/// transaction or a transaction left open; otherwise only without
/// `continue_on_error`. A late completion stops it too.
pub async fn run(
    session: &dyn Session,
    plan: &ScriptPlan,
    opts: &ExecOptions,
    continue_on_error: bool,
    cancel: &RunCancel,
    sink: &mut dyn RunSink,
) -> RunReport {
    let opts = ExecOptions {
        stop: cancel.signal(),
        ..opts.clone()
    };
    let mut report = RunReport {
        stop: None,
        stmts: Vec::new(),
        reconnects: 0,
    };
    let mut dialect = plan.dialect;
    let mut shown_tx = session.tx_state();
    let mut reconnects = session.reconnects();

    for (i, stmt) in plan.stmts.iter().enumerate() {
        if cancel.is_cancelled() {
            report.stop = Some(StopReason::Cancelled);
            break;
        }
        // A `SET sql_mode`, or a reconnect that reset it, may have changed the lexing.
        // Checked before every statement, also after one that failed (and the run
        // went on with `continue_on_error`): a failed statement can change the mode
        // too, and the MySQL driver re-reads a mode it could not read after a
        // failure (a stale mode) inside its next `execute_one`, so `dialect()` is
        // only up to date once that call returned.
        let now = session.dialect();
        if now != dialect {
            if !same_rest(plan, i, now) {
                report.stop = Some(StopReason::DialectChanged { index: stmt.index });
                break;
            }
            dialect = now;
        }
        let before = session.tx_state();
        if before != shown_tx {
            shown_tx = before;
            sink.tx_state(before);
        }
        if before == TxState::Aborted && !ends_aborted(stmt, dialect) {
            report.stop = Some(StopReason::TxAborted);
            break;
        }

        sink.statement_start(stmt);
        let rows: &mut dyn RowSink = &mut *sink;
        let result = session
            .execute_one(&stmt.text, stmt.kind, &opts, rows)
            .await;
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
                let stop = stop_after_error(&err, after, continue_on_error, stmt.index);
                let position = error_offset(stmt, &err);
                let mut app = AppError::from(err);
                app.tx_state = Some(after.as_str().into());
                app.position = position;
                (Err(app), stop)
            }
        };
        sink.statement_end(stmt, &result);
        if fresh > 0 && !lost {
            sink.notice(stmt.index, &reconnect_notice(report.reconnects));
        }
        if result.is_ok()
            && before == TxState::Aborted
            && stmt.kind == StatementKind::Transaction(TxControl::Commit)
        {
            sink.notice(stmt.index, ROLLED_BACK_NOTICE);
        }
        if after != shown_tx {
            shown_tx = after;
            sink.tx_state(after);
        }
        report.stmts.push(StmtReport {
            index: stmt.index,
            result,
        });
        if stop.is_some() {
            report.stop = stop;
            break;
        }
    }
    report
}

/// The stop after statement `index` failed with `err`, leaving the session in `after`.
fn stop_after_error(
    err: &DbError,
    after: TxState,
    continue_on_error: bool,
    index: u32,
) -> Option<StopReason> {
    let error = Some(StopReason::Error { index });
    match err.unstaged() {
        DbError::Cancelled { .. } => Some(StopReason::Cancelled),
        DbError::ConnectionLost { .. } => error,
        DbError::Timeout {
            outcome_unknown,
            was_in_tx,
            ..
        } if *outcome_unknown || *was_in_tx => error,
        _ if after != TxState::Idle => error,
        _ if continue_on_error => None,
        _ => error,
    }
}

/// What PostgreSQL still accepts in an aborted transaction: `ROLLBACK`, `ABORT`,
/// `ROLLBACK TO [SAVEPOINT]`, `COMMIT` and `END` (which roll it back).
fn ends_aborted(stmt: &PlannedStmt, dialect: Dialect) -> bool {
    matches!(
        stmt.kind,
        StatementKind::Transaction(TxControl::Rollback | TxControl::Commit)
    ) || is_rollback_to(&stmt.text, dialect)
}

/// Whether splitting with `dialect` gives the planned statements `i..` again (same
/// boundaries, same kinds) and, for a guarded plan, the guard judges each of them as
/// planned under `dialect` (the guard lexes the text, so a mode change can change a
/// verdict without changing a boundary or a kind).
///
/// Two readings are accepted: the text after the last statement run (`i - 1`) split
/// on its own, as the spec says; or the whole source split again, which keeps a
/// MySQL `DELIMITER` that is in effect there. Either way only the statements up to
/// the end of the last planned one are compared, so a Cursor plan (one statement
/// out of a longer text) checks just its own statement.
fn same_rest(plan: &ScriptPlan, i: usize, dialect: Dialect) -> bool {
    let rest = &plan.stmts[i..];
    let Some(last) = rest.last() else {
        return true;
    };
    let from = match i.checked_sub(1) {
        Some(prev) => plan.stmts[prev].source_range.1,
        None => rest[0].source_range.0,
    };
    let to = last.source_range.1;
    let want: Vec<_> = rest
        .iter()
        .map(|s| (s.source_range.0, s.source_range.1, s.kind))
        .collect();
    let resplit = |base: usize| -> Vec<(usize, usize, StatementKind)> {
        significant(&plan.source[base..], dialect)
            .into_iter()
            .filter(|s| base + s.end > from && base + s.start < to)
            .map(|s| (base + s.start, base + s.end, classify(&s.text, dialect)))
            .collect()
    };
    let same_bounds = resplit(from) == want || (from != 0 && resplit(0) == want);
    same_bounds
        && plan.guard.is_none_or(|ctx| {
            rest.iter()
                .all(|s| stmt_verdict(&s.text, s.kind, dialect, plan.explain, ctx) == s.verdict)
        })
}

/// The editor offset (UTF-16) of the error `err` of `stmt`, if the server said where:
///
/// - PostgreSQL `position` (1-based chars of `stmt.text`): that char.
/// - MySQL "… near '<snippet>' at line N": where the snippet's first 20 chars start
///   on line N of `stmt.text`, else the start of line N (the end of the statement for
///   an empty snippet: the input ended too early).
///
/// `err` is the RAW driver error, as `execute_one` returned it: `run` calls this
/// before mapping it to the `AppError` it reports, whose `position` is then already
/// the editor offset. Taking a `DbError` keeps it from being applied twice.
pub fn error_offset(stmt: &PlannedStmt, err: &DbError) -> Option<u32> {
    let DbError::Server {
        message, position, ..
    } = err.unstaged()
    else {
        return None;
    };
    let text = stmt.text.as_str();
    let byte = match position {
        Some(pos) => {
            let skip = pos.saturating_sub(1) as usize;
            text.char_indices().nth(skip).map_or(text.len(), |(b, _)| b)
        }
        None => mysql_near(text, message)?,
    };
    Some(stmt.range.0 + Utf16Map::new(text).to_u16(byte))
}

/// Byte offset in `text` that a MySQL "near '…' at line N" `message` points at.
fn mysql_near(text: &str, message: &str) -> Option<usize> {
    const NEAR: &str = "near '";
    const AT_LINE: &str = "' at line ";
    let start = message.find(NEAR)? + NEAR.len();
    let tail = &message[start..];
    let end = tail.rfind(AT_LINE)?;
    let snippet = &tail[..end];
    let digits: String = tail[end + AT_LINE.len()..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    let line: usize = digits.parse().ok()?;
    let line_start = match line {
        0 => return None,
        1 => 0,
        n => text.match_indices('\n').nth(n - 2)?.0 + 1,
    };
    let line_end = text[line_start..]
        .find('\n')
        .map_or(text.len(), |e| line_start + e);
    if snippet.is_empty() {
        return Some(text.trim_end().len().max(line_start));
    }
    let cut = snippet
        .char_indices()
        .nth(20)
        .map_or(snippet.len(), |(b, _)| b);
    match text[line_start..].find(&snippet[..cut]) {
        Some(off) if line_start + off <= line_end => Some(line_start + off),
        _ => Some(line_start),
    }
}
