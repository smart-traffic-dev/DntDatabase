//! Transaction state of a session and how each statement changes it (P2 spec §4.2).

use serde::{Deserialize, Serialize};

use crate::server::SqlKind;
use crate::sql::{leading_keywords, Dialect, StatementKind, TxControl};

/// Transaction state of a session as the query layer sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum TxState {
    /// No transaction is open (autocommit).
    Idle,
    /// A transaction is open (or MySQL `autocommit = 0` is in effect).
    Active,
    /// PostgreSQL only: a statement failed inside a transaction block; the server
    /// rejects everything but `ROLLBACK [TO SAVEPOINT]` / `COMMIT` until it ends.
    Aborted,
}

impl TxState {
    pub fn as_str(self) -> &'static str {
        match self {
            TxState::Idle => "idle",
            TxState::Active => "active",
            TxState::Aborted => "aborted",
        }
    }
}

/// How a statement ended, as [`next_tx_state`] needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StmtResult<'a> {
    Ok,
    /// The server answered with an error (a timeout or cancel is `57014` on PG).
    /// `sqlstate`: the SQLSTATE when known; `code`: the MySQL error number.
    Failed {
        sqlstate: Option<&'a str>,
        code: Option<u16>,
    },
    /// The connection was lost: the server rolled any open transaction back.
    Lost,
}

/// InnoDB deadlock: the server rolled the whole transaction back.
const MYSQL_DEADLOCK: u16 = 1213;

/// MySQL errors raised while parsing: the statement never ran, so a DDL statement that
/// fails with one did not commit implicitly (`ER_PARSE_ERROR`, `ER_SYNTAX_ERROR`).
const MYSQL_PARSE_ERRORS: [u16; 2] = [1064, 1149];

/// Whether the MySQL session's `autocommit` is off after `kind` ended with `result`,
/// given that it was `prev_off` before. Only a successful `SET autocommit = …`
/// changes it; a lost connection keeps it (the driver re-applies it on the new
/// connection). Always `false` on PostgreSQL, which has no such mode.
pub fn next_autocommit_off(
    engine: SqlKind,
    prev_off: bool,
    kind: StatementKind,
    result: StmtResult<'_>,
) -> bool {
    match engine {
        SqlKind::MySql => {}
        SqlKind::Postgres => return false,
    }
    match (kind, result) {
        (StatementKind::Transaction(TxControl::AutocommitOff), StmtResult::Ok) => true,
        (StatementKind::Transaction(TxControl::AutocommitOn), StmtResult::Ok) => false,
        _ => prev_off,
    }
}

/// The transaction state after `stmt` (of kind `kind`, classified for `engine`) ended
/// with `result` on a session that was in state `prev`. `autocommit_off`: MySQL only,
/// the session's `autocommit` was off before `stmt` (track it with
/// [`next_autocommit_off`]); ignored on PostgreSQL.
///
/// - `BEGIN` / `START TRANSACTION`, and MySQL `SET autocommit = 0`, open a transaction
///   when they succeed; MySQL `SET autocommit = 1` commits it.
/// - `COMMIT` / `ROLLBACK` / `END` / `ABORT` (and PG `PREPARE TRANSACTION`) end it,
///   unless `… AND CHAIN` opens the next one at once. When one fails it still ended the
///   transaction (the server rolled it back), except a rejection of class `42` (syntax
///   error, access rule) which left a block as it was: `Aborted` on PG (any error
///   aborts a PG block), unchanged on MySQL.
/// - PG: `ROLLBACK TO [SAVEPOINT]` that succeeds in an aborted block makes it `Active`
///   again; any other failure inside a block (`57014` included) aborts it.
/// - MySQL: DDL and the other implicit-commit statements ([`mysql_implicit_commit`])
///   commit, even when they then fail, unless they failed to parse; a deadlock
///   (`1213`) rolls the transaction back; other failures keep the state.
/// - MySQL with `autocommit` off: the server opens a transaction for any statement,
///   so whatever would leave the session `Idle` leaves it `Active` (until
///   `SET autocommit = 1`).
/// - A lost connection always leaves the session `Idle`.
///
/// The dialect's MySQL `sql_mode` flags are ignored: they only change how quoted text
/// is lexed, and every keyword read here (`AND CHAIN`, `ROLLBACK … TO`,
/// `PREPARE TRANSACTION`, the implicit-commit statements) leads the statement, before
/// any quoted text.
pub fn next_tx_state(
    engine: SqlKind,
    prev: TxState,
    autocommit_off: bool,
    kind: StatementKind,
    stmt: &str,
    result: StmtResult<'_>,
) -> TxState {
    let state = tx_state_after(engine, prev, kind, stmt, result);
    let off_after = next_autocommit_off(engine, autocommit_off, kind, result);
    if state == TxState::Idle && off_after && result != StmtResult::Lost {
        TxState::Active
    } else {
        state
    }
}

/// [`next_tx_state`] without the MySQL autocommit mode.
fn tx_state_after(
    engine: SqlKind,
    prev: TxState,
    kind: StatementKind,
    stmt: &str,
    result: StmtResult<'_>,
) -> TxState {
    let dialect = Dialect::for_kind(engine);
    let (implicit_commit, pg) = match engine {
        SqlKind::MySql => (mysql_implicit_commit(kind, stmt), false),
        SqlKind::Postgres => (false, true),
    };
    let ends_tx = matches!(
        kind,
        StatementKind::Transaction(TxControl::Commit | TxControl::Rollback)
    ) || (pg && is_prepare_transaction(stmt));
    match result {
        StmtResult::Lost => TxState::Idle,
        StmtResult::Ok => match kind {
            _ if ends_tx => {
                if is_and_chain(stmt, dialect) {
                    TxState::Active
                } else {
                    TxState::Idle
                }
            }
            StatementKind::Transaction(TxControl::Begin) => TxState::Active,
            StatementKind::Transaction(TxControl::AutocommitOff) if !pg => TxState::Active,
            StatementKind::Transaction(TxControl::AutocommitOn) if !pg => TxState::Idle,
            _ if implicit_commit => TxState::Idle,
            _ if pg && prev == TxState::Aborted && is_rollback_to(stmt, dialect) => TxState::Active,
            _ => prev,
        },
        StmtResult::Failed { sqlstate, code } => {
            if ends_tx {
                let rejected = sqlstate.is_some_and(|s| s.starts_with("42"));
                return match (rejected, prev) {
                    (false, _) | (true, TxState::Idle) => TxState::Idle,
                    (true, _) if pg => TxState::Aborted,
                    (true, prev) => prev,
                };
            }
            if pg {
                match prev {
                    TxState::Idle => TxState::Idle,
                    TxState::Active | TxState::Aborted => TxState::Aborted,
                }
            } else if code == Some(MYSQL_DEADLOCK)
                || (implicit_commit && !code.is_some_and(|c| MYSQL_PARSE_ERRORS.contains(&c)))
            {
                TxState::Idle
            } else {
                prev
            }
        }
    }
}

/// MySQL statements that commit the open transaction before they run: DDL (per
/// `classify`), and `LOCK TABLES`, `ANALYZE` / `CHECK` / `OPTIMIZE` / `REPAIR TABLE`,
/// `FLUSH`, `RESET` (except `RESET QUERY CACHE` and `RESET PERSIST`), `CACHE INDEX` and
/// `LOAD INDEX INTO CACHE`.
///
/// `UNLOCK TABLES` commits only while `LOCK TABLES` is in effect, which is not tracked
/// here, so it keeps the state.
fn mysql_implicit_commit(kind: StatementKind, stmt: &str) -> bool {
    if kind == StatementKind::Ddl {
        return true;
    }
    let words = leading_keywords(stmt, Dialect::MYSQL, 3);
    let w: Vec<&str> = words.iter().map(String::as_str).collect();
    matches!(
        w.as_slice(),
        ["LOCK", "TABLE" | "TABLES", ..]
            | ["ANALYZE" | "CHECK" | "OPTIMIZE" | "REPAIR", "TABLE", ..]
            | [
                "ANALYZE" | "OPTIMIZE" | "REPAIR",
                "NO_WRITE_TO_BINLOG" | "LOCAL",
                "TABLE"
            ]
            | ["FLUSH", ..]
            | ["CACHE", "INDEX", ..]
            | ["LOAD", "INDEX", "INTO"]
    ) || (w.first() == Some(&"RESET")
        && !matches!(
            w.as_slice(),
            ["RESET", "QUERY", "CACHE"] | ["RESET", "PERSIST", ..]
        ))
}

/// `ROLLBACK [WORK | TRANSACTION] TO …` (comments allowed before and between).
pub fn is_rollback_to(stmt: &str, dialect: Dialect) -> bool {
    let words = leading_keywords(stmt, dialect, 3);
    let w: Vec<&str> = words.iter().map(String::as_str).collect();
    matches!(
        w.as_slice(),
        ["ROLLBACK", "TO", ..] | ["ROLLBACK", "WORK" | "TRANSACTION", "TO"]
    )
}

/// PG `PREPARE TRANSACTION 'id'` ends the current transaction (it is handed to 2PC).
fn is_prepare_transaction(stmt: &str) -> bool {
    leading_keywords(stmt, Dialect::Postgres, 2) == ["PREPARE", "TRANSACTION"]
}

/// `COMMIT` / `END` / `ROLLBACK` / `ABORT … AND CHAIN`: a new transaction starts at once.
fn is_and_chain(stmt: &str, dialect: Dialect) -> bool {
    let words = leading_keywords(stmt, dialect, 5);
    words.windows(2).any(|w| w[0] == "AND" && w[1] == "CHAIN")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::classify;
    use SqlKind::{MySql, Postgres};
    use TxState::{Aborted, Active, Idle};

    fn failed(sqlstate: &str) -> StmtResult<'_> {
        StmtResult::Failed {
            sqlstate: Some(sqlstate),
            code: None,
        }
    }

    fn mysql_failed(code: u16) -> StmtResult<'static> {
        StmtResult::Failed {
            sqlstate: Some("HY000"),
            code: Some(code),
        }
    }

    /// `next_tx_state` with the statement classified for `engine`, autocommit on.
    fn next(engine: SqlKind, prev: TxState, stmt: &str, result: StmtResult<'_>) -> TxState {
        let kind = classify(stmt, Dialect::for_kind(engine));
        next_tx_state(engine, prev, false, kind, stmt, result)
    }

    /// Runs `script` (each statement with its result) on a fresh session, tracking the
    /// autocommit mode as a driver does; returns the state after each statement.
    fn run_script(engine: SqlKind, script: &[(&str, StmtResult<'_>)]) -> Vec<TxState> {
        let (mut state, mut ac_off) = (Idle, false);
        script
            .iter()
            .map(|&(stmt, result)| {
                let kind = classify(stmt, Dialect::for_kind(engine));
                state = next_tx_state(engine, state, ac_off, kind, stmt, result);
                ac_off = next_autocommit_off(engine, ac_off, kind, result);
                state
            })
            .collect()
    }

    #[test]
    fn mysql_autocommit_off_survives_commit() {
        let ok = StmtResult::Ok;
        assert_eq!(
            run_script(
                MySql,
                &[
                    ("SET autocommit=0", ok),
                    ("COMMIT", ok),
                    ("INSERT INTO t VALUES (1)", ok)
                ]
            ),
            [Active, Active, Active]
        );
        // MySQL opens a transaction on any statement while autocommit is off.
        assert_eq!(
            run_script(
                MySql,
                &[("SET autocommit=0", ok), ("ROLLBACK", ok), ("SELECT 1", ok)]
            ),
            [Active, Active, Active]
        );
        // Implicit commits and a deadlock do not turn autocommit back on.
        assert_eq!(
            run_script(
                MySql,
                &[
                    ("SET autocommit=0", ok),
                    ("CREATE TABLE t (a int)", ok),
                    ("UPDATE t SET a = 1", mysql_failed(1213)),
                    ("SELECT 1", ok),
                ]
            ),
            [Active, Active, Active, Active]
        );
        // `SET autocommit = 1` commits and turns it back on.
        assert_eq!(
            run_script(
                MySql,
                &[
                    ("SET autocommit=0", ok),
                    ("SET autocommit=1", ok),
                    ("SELECT 1", ok)
                ]
            ),
            [Active, Idle, Idle]
        );
        // A failed SET leaves the mode as it was.
        assert_eq!(
            run_script(
                MySql,
                &[("SET autocommit=0", mysql_failed(1231)), ("SELECT 1", ok)]
            ),
            [Idle, Idle]
        );
        // Losing the connection ends the transaction (the driver re-applies the mode).
        assert_eq!(
            run_script(
                MySql,
                &[("SET autocommit=0", ok), ("SELECT 1", StmtResult::Lost)]
            ),
            [Active, Idle]
        );
        // PostgreSQL has no autocommit mode.
        assert!(!next_autocommit_off(
            Postgres,
            false,
            classify("SET autocommit = 0", Dialect::Postgres),
            StmtResult::Ok
        ));
    }

    #[test]
    fn mysql_ddl_parse_error_does_not_commit() {
        for code in [1064, 1149] {
            let parse = StmtResult::Failed {
                sqlstate: Some("42000"),
                code: Some(code),
            };
            assert_eq!(
                next(MySql, Active, "CREATE TABLE (", parse),
                Active,
                "{code}"
            );
            assert_eq!(next(MySql, Active, "LOCK TABLES", parse), Active, "{code}");
        }
        assert_eq!(
            next(MySql, Active, "DROP TABLE nope", mysql_failed(1051)),
            Idle
        );
    }

    #[test]
    fn mysql_implicit_commit_statements() {
        let cases = [
            ("LOCK TABLES t WRITE", true),
            ("lock table t read", true),
            ("ANALYZE TABLE t", true),
            ("ANALYZE NO_WRITE_TO_BINLOG TABLE t", true),
            ("ANALYZE LOCAL TABLE t", true),
            ("CHECK TABLE t", true),
            ("FLUSH PRIVILEGES", true),
            ("FLUSH TABLES", true),
            ("RESET MASTER", true),
            ("RESET QUERY CACHE", false),
            ("RESET PERSIST", false),
            ("CACHE INDEX t IN hot", true),
            ("LOAD INDEX INTO CACHE t", true),
            ("/* c */ OPTIMIZE TABLE t", true),
            // Not implicit commits.
            ("UNLOCK TABLES", false),
            ("ANALYZE SELECT 1", false),
            ("CHECKSUM TABLE t", false),
            ("LOAD DATA INFILE 'x' INTO TABLE t", false),
            ("SELECT 'LOCK TABLES'", false),
        ];
        for (sql, commits) in cases {
            let want = if commits { Idle } else { Active };
            assert_eq!(next(MySql, Active, sql, StmtResult::Ok), want, "{sql} ok");
            assert_eq!(
                next(MySql, Active, sql, mysql_failed(1146)),
                want,
                "{sql} failed"
            );
            // PG has no implicit commit.
            assert_eq!(
                next(Postgres, Active, sql, StmtResult::Ok),
                Active,
                "{sql} pg"
            );
        }
    }

    #[test]
    fn tx_state_strings_match_serde() {
        for s in [Idle, Active, Aborted] {
            assert_eq!(serde_json::to_value(s).unwrap(), s.as_str());
        }
    }

    #[test]
    fn tx_begin_activates() {
        for engine in [MySql, Postgres] {
            for sql in [
                "BEGIN",
                "START TRANSACTION",
                "/* x */ start transaction read only",
            ] {
                assert_eq!(next(engine, Idle, sql, StmtResult::Ok), Active, "{sql}");
            }
            // A BEGIN that failed outside a transaction opened nothing.
            assert_eq!(next(engine, Idle, "BEGIN x", failed("42601")), Idle);
        }
    }

    #[test]
    fn tx_commit_rollback_end_abort_go_idle() {
        for sql in [
            "COMMIT",
            "ROLLBACK",
            "END",
            "ABORT",
            "commit work",
            "COMMIT AND NO CHAIN",
        ] {
            assert_eq!(next(Postgres, Active, sql, StmtResult::Ok), Idle, "{sql}");
        }
        for sql in ["COMMIT", "ROLLBACK", "ROLLBACK WORK"] {
            assert_eq!(next(MySql, Active, sql, StmtResult::Ok), Idle, "{sql}");
        }
        // AND CHAIN starts the next transaction at once.
        for engine in [MySql, Postgres] {
            for sql in ["COMMIT AND CHAIN", "/* x */ ROLLBACK AND CHAIN"] {
                assert_eq!(next(engine, Active, sql, StmtResult::Ok), Active, "{sql}");
            }
        }
        // A failed COMMIT (deferred constraint, serialization failure, cancel) ended it.
        for st in ["23503", "40001", "57014"] {
            assert_eq!(next(Postgres, Active, "COMMIT", failed(st)), Idle, "{st}");
        }
        assert_eq!(
            next(
                Postgres,
                Active,
                "COMMIT",
                StmtResult::Failed {
                    sqlstate: None,
                    code: None
                }
            ),
            Idle
        );
        // PREPARE TRANSACTION hands the transaction to 2PC, and a failed one rolls it back.
        assert_eq!(
            next(Postgres, Active, "PREPARE TRANSACTION 'x'", StmtResult::Ok),
            Idle
        );
        assert_eq!(
            next(Postgres, Active, "PREPARE TRANSACTION 'x'", failed("XX000")),
            Idle
        );
    }

    #[test]
    fn pg_error_in_tx_aborts() {
        assert_eq!(
            next(
                Postgres,
                Active,
                "INSERT INTO t VALUES (1)",
                failed("23505")
            ),
            Aborted
        );
        assert_eq!(
            next(Postgres, Active, "SELECT 1/0", failed("22012")),
            Aborted
        );
        assert_eq!(
            next(Postgres, Aborted, "SELECT 1", failed("25P02")),
            Aborted
        );
        // A failed ROLLBACK TO (no such savepoint) keeps the block aborted.
        assert_eq!(
            next(Postgres, Aborted, "ROLLBACK TO s", failed("3B001")),
            Aborted
        );
        // Outside a block nothing is open to abort.
        assert_eq!(next(Postgres, Idle, "SELECT 1/0", failed("22012")), Idle);
    }

    #[test]
    fn pg_timeout_in_tx_aborts() {
        assert_eq!(
            next(Postgres, Active, "SELECT pg_sleep(5)", failed("57014")),
            Aborted
        );
        assert_eq!(
            next(Postgres, Idle, "SELECT pg_sleep(5)", failed("57014")),
            Idle
        );
    }

    #[test]
    fn mysql_error_in_tx_keeps_active() {
        assert_eq!(
            next(
                MySql,
                Active,
                "INSERT INTO t VALUES (1)",
                mysql_failed(1062)
            ),
            Active
        );
        // A lock wait timeout only rolls the statement back.
        assert_eq!(
            next(MySql, Active, "UPDATE t SET a = 1", mysql_failed(1205)),
            Active
        );
        assert_eq!(next(MySql, Idle, "SELECT x", mysql_failed(1054)), Idle);
    }

    #[test]
    fn pg_rollback_to_when_aborted_reactivates() {
        for sql in [
            "ROLLBACK TO s",
            "ROLLBACK TO SAVEPOINT s",
            "rollback transaction to s",
        ] {
            assert_eq!(
                next(Postgres, Aborted, sql, StmtResult::Ok),
                Active,
                "{sql}"
            );
            assert_eq!(next(Postgres, Active, sql, StmtResult::Ok), Active, "{sql}");
        }
        assert_eq!(
            next(Postgres, Active, "SAVEPOINT s", StmtResult::Ok),
            Active
        );
    }

    #[test]
    fn pg_commit_when_aborted_goes_idle() {
        // The server answers ROLLBACK: the block is over.
        for sql in ["COMMIT", "END", "ROLLBACK", "ABORT"] {
            assert_eq!(next(Postgres, Aborted, sql, StmtResult::Ok), Idle, "{sql}");
        }
    }

    #[test]
    fn lost_connection_goes_idle() {
        for engine in [MySql, Postgres] {
            for prev in [Idle, Active, Aborted] {
                for sql in [
                    "BEGIN",
                    "SELECT 1",
                    "COMMIT AND CHAIN",
                    "SET autocommit = 0",
                ] {
                    assert_eq!(next(engine, prev, sql, StmtResult::Lost), Idle, "{sql}");
                }
            }
        }
    }

    #[test]
    fn mysql_ddl_commits_implicitly() {
        assert_eq!(
            next(MySql, Active, "CREATE TABLE t (a int)", StmtResult::Ok),
            Idle
        );
        assert_eq!(
            next(MySql, Active, "DROP TABLE nope", mysql_failed(1051)),
            Idle
        );
        // PG DDL is transactional.
        assert_eq!(
            next(Postgres, Active, "CREATE TABLE t (a int)", StmtResult::Ok),
            Active
        );
    }

    #[test]
    fn mysql_deadlock_ends_tx() {
        assert_eq!(
            next(MySql, Active, "UPDATE t SET a = 1", mysql_failed(1213)),
            Idle
        );
    }

    #[test]
    fn autocommit_off_activates() {
        assert_eq!(
            next(MySql, Idle, "SET autocommit = 0", StmtResult::Ok),
            Active
        );
        assert_eq!(
            next(MySql, Active, "SET autocommit = 1", StmtResult::Ok),
            Idle
        );
        // PostgreSQL has no autocommit setting: the server rejects it.
        assert_eq!(
            next(Postgres, Idle, "SET autocommit = 0", StmtResult::Ok),
            Idle
        );
        assert_eq!(
            next(Postgres, Idle, "SET autocommit = 0", failed("42704")),
            Idle
        );
    }

    #[test]
    fn pg_class_42_commit_failure_stays_aborted() {
        // Rejected before it ran: the block is still there, aborted by the error.
        assert_eq!(
            next(Postgres, Aborted, "COMMIT AND x", failed("42601")),
            Aborted
        );
        assert_eq!(
            next(Postgres, Active, "COMMIT AND x", failed("42601")),
            Aborted
        );
        assert_eq!(
            next(Postgres, Active, "PREPARE TRANSACTION x", failed("42601")),
            Aborted
        );
        // Outside a block there is nothing to keep.
        assert_eq!(next(Postgres, Idle, "COMMIT AND x", failed("42601")), Idle);
        // MySQL keeps the transaction open.
        assert_eq!(
            next(
                MySql,
                Active,
                "COMMIT x",
                StmtResult::Failed {
                    sqlstate: Some("42000"),
                    code: Some(1064)
                }
            ),
            Active
        );
    }

    #[test]
    fn is_rollback_to_forms() {
        for d in [Dialect::MYSQL, Dialect::Postgres] {
            assert!(is_rollback_to("ROLLBACK TO s", d));
            assert!(is_rollback_to("rollback work to savepoint s", d));
            assert!(is_rollback_to("/* c */ ROLLBACK TRANSACTION TO s", d));
            assert!(!is_rollback_to("ROLLBACK", d));
            assert!(!is_rollback_to("ROLLBACK WORK", d));
            assert!(!is_rollback_to("ROLLBACK AND CHAIN", d));
            assert!(!is_rollback_to("SELECT 'ROLLBACK TO s'", d));
        }
    }
}
