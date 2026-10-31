//! F1 (DB): a stray `DELIMITER $$` makes one planned statement hold several pieces;
//! the run must still report the transaction state the server is really in.

use dnt_core::driver::{Driver, ExecOptions, ResultSummary, RowSink, Session, StmtOutcome};
use dnt_core::error::AppError;
use dnt_core::tx::TxState;
use dnt_core::value::{ColumnMeta, Value};
use dnt_driver_mysql::MySqlDriver;
use dnt_query::{plan, run, PlannedStmt, RunCancel, RunSink, Target as Scope};
use dnt_testkit::{require_db, Target, FIXTURE_DB};

#[derive(Default)]
struct Rec {
    tx: Vec<TxState>,
    ok: Vec<bool>,
}

impl RowSink for Rec {
    fn result_start(&mut self, _cols: &[ColumnMeta]) {}
    fn rows(&mut self, _batch: Vec<Vec<Value>>) {}
    fn result_end(&mut self, _summary: &ResultSummary) {}
}

impl RunSink for Rec {
    fn statement_start(&mut self, _stmt: &PlannedStmt) {}
    fn statement_end(&mut self, _stmt: &PlannedStmt, result: &Result<StmtOutcome, AppError>) {
        self.ok.push(result.is_ok());
    }
    fn notice(&mut self, _index: u32, _message: &str) {}
    fn tx_state(&mut self, state: TxState) {
        self.tx.push(state);
    }
}

async fn run_text(s: &dyn Session, text: &str) -> Rec {
    let s = s.sql().unwrap();
    let p = plan(text, Scope::All, s.dialect());
    let cancel = RunCancel::new(s.cancel_handle());
    let mut rec = Rec::default();
    run(s, &p, &ExecOptions::default(), false, &cancel, &mut rec).await;
    rec
}

#[tokio::test]
async fn delimiter_left_on_reports_the_servers_tx_state() {
    require_db!();
    for t in Target::mysql_family() {
        let s = MySqlDriver
            .connect(&t.resolved(Some(FIXTURE_DB)))
            .await
            .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        let setup = run_text(
            &*s,
            "DROP TABLE IF EXISTS f1_run; CREATE TABLE f1_run (a int) ENGINE=InnoDB",
        )
        .await;
        assert_eq!(setup.ok, [true, true], "{t:?}");

        // One planned statement (`Write`): the BEGIN piece opened a transaction.
        let rec = run_text(&*s, "DELIMITER $$\nBEGIN; INSERT INTO f1_run VALUES (1)$$").await;
        assert_eq!(rec.ok, [true], "{t:?}");
        assert_eq!(rec.tx, [TxState::Active], "{t:?}");
        assert_eq!(s.sql().unwrap().tx_state(), TxState::Active, "{t:?}");

        // `SELECT 1; COMMIT` ends it.
        let rec = run_text(&*s, "DELIMITER $$\nSELECT 1; COMMIT$$").await;
        assert_eq!(rec.ok, [true], "{t:?}");
        assert_eq!(rec.tx, [TxState::Idle], "{t:?}");

        // `COMMIT; UPDATE …` (planned `Write`) after a BEGIN ends it too.
        let rec = run_text(
            &*s,
            "BEGIN;\nDELIMITER $$\nCOMMIT; UPDATE f1_run SET a = 2$$",
        )
        .await;
        assert_eq!(rec.ok, [true, true], "{t:?}");
        assert_eq!(rec.tx, [TxState::Active, TxState::Idle], "{t:?}");

        run_text(&*s, "DROP TABLE f1_run").await;
        s.close().await;
    }
}
