//! Transaction state of a session and how each statement changes it (P2 spec §4.2).

use serde::{Deserialize, Serialize};

use crate::server::DbKind;
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

/// The transaction state after `stmt` (of kind `kind`, classified for `engine`) ended
/// with `result` on a session that was in state `prev`.
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
/// - MySQL: DDL commits implicitly, even when it then fails; a deadlock (`1213`) rolls
///   the transaction back; other failures keep the state.
/// - A lost connection always leaves the session `Idle`.
pub fn next_tx_state(
    engine: DbKind,
    prev: TxState,
    kind: StatementKind,
    stmt: &str,
    result: StmtResult<'_>,
) -> TxState {
    let dialect = Dialect::for_kind(engine);
    let pg = engine == DbKind::Postgres;
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
            StatementKind::Ddl if !pg => TxState::Idle,
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
            } else if kind == StatementKind::Ddl || code == Some(MYSQL_DEADLOCK) {
                TxState::Idle
            } else {
                prev
            }
        }
    }
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
    use DbKind::{MySql, Postgres};
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

    /// `next_tx_state` with the statement classified for `engine`.
    fn next(engine: DbKind, prev: TxState, stmt: &str, result: StmtResult<'_>) -> TxState {
        let kind = classify(stmt, Dialect::for_kind(engine));
        next_tx_state(engine, prev, kind, stmt, result)
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
