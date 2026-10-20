# P3 data editing: final verification (Task 17)

Branch `feat/p3-data`, worktree `D:\dev\dnt-p3`. This is an acceptance run against spec §5.6, with the output of every
command in the Task 17 list. Nothing here is a code review: it records what was run and which test proves which bullet.

## 1. Command results

All Rust commands ran with `CARGO_HOME=D:\dev\cargo`, `RUSTUP_HOME=D:\dev\rustup`, one at a time (no other heavy job in
parallel). The Docker test containers (MySQL 5.7/8.0/8.4, MariaDB 10.6/11.4, PostgreSQL 12/17) were up.

| Command | Result |
|---|---|
| `cargo fmt --all --check` | exit 0, no diff |
| `cargo clippy --workspace --all-targets -- -D warnings` | exit 0, no warnings |
| `cargo test --workspace` (no DB) | exit 0; 1020 passed, 0 failed (DB tests return early without `DNT_TEST_DB`) |
| `$env:DNT_TEST_DB='1'; cargo test --workspace` | exit 0; 1020 passed, 0 failed (the DB tests really ran: e.g. the `driver-mysql` and `driver-pg` execute binaries take 22 s each). No test needed a rerun, no flake seen |
| `pnpm --dir ui lint` (eslint + `tsc --noEmit`) | exit 0 |
| `pnpm --dir ui test` | 94 files, 1058 tests passed, 0 failed |
| `pnpm e2e:web` | **Not run: the script does not exist.** `e2e-web` is not on this branch (controller ruling R1). Its scenarios are Vitest tests instead, see section 2 |
| `pnpm --dir e2e build:app` | exit 0 (UI build plus `cargo build --features tauri/custom-protocol` into `target/e2e`) |
| `pnpm --dir e2e test` (tauri-driver 4444, msedgedriver, WebView2 155) | 4 spec files passed: `smoke` 1, `postgres` 1, `query` 2, `data-edit` 2 (MySQL 8.0 and PostgreSQL 17) |

The two Rust totals are equal because the DB tests count as passed in both runs.

### Smoke E2E notes

- Cleanup is guaranteed: an `after` hook drops this run's own `p3_smoke_<uuid8>` with `DROP TABLE IF EXISTS` through
  `docker exec` of the container's `mysql`/`psql` client, independent of the UI. Checked by forcing a failure right after
  the table was created: no table was left on MySQL 8 or PG 17.
- New scenario `e2e/data-edit.e2e.ts`, registered in `e2e/wdio.conf.ts`. Per engine it creates a Local-environment
  connection, creates `p3_smoke_<uuid8>` (`id INT PRIMARY KEY, label VARCHAR(40)`, one row `before`) from a query tab,
  opens the table's Data tab, edits `label` to `after`, reviews the real `UPDATE` in the dialog, Confirms, reads the row
  back from a second query tab (`WHERE label = 'after'` returns 1 row) and drops the table. After the final run no
  `p3_smoke_*` or `e2e_*` table is left in either `dnt_fixture`; leftovers of the failed iterations while writing the
  scenario were dropped by hand (only tables named `p3_smoke_*` and `e2e_guard`).
- The grid is a canvas, so a cell cannot be edited by typing into it through WebDriver. The scenario selects the cell by
  a click at an offset and presses Shift+Enter, which opens the DOM value editor (`Value` textbox, `Save`). The staged
  edit, preview and apply paths are the real ones; only the in-canvas overlay editor is not driven. The click offset
  (120 px, 58 px from the canvas corner) depends on the default column widths and row height of the grid.
- Existing `e2e/query.e2e.ts` failed against P3 and was fixed: its cleanup `DROP TABLE e2e_guard` on a Production
  connection now requires typing the table name (Task 12), so `run-confirm-run` stayed disabled. The spec now asserts that
  Run is disabled and types the name (`run-confirm-name`) before running. That edit also shows, end to end, that a
  Production DROP needs the typed name.

## 2. Browser E2E replaced by Vitest (ruling R1)

The Step 1 flows, and the existing test that proves each one. Only the first flow had no single end-to-end walk, so one
was added.

| Flow | Test (file, name) |
|---|---|
| filter, edit a cell, preview, apply, page reloads | **new** `ui/src/tabs/DataView.flow.test.tsx`, "walks the whole path and keeps the filter on the reload". Parts also in `ui/src/tabs/DataView.test.tsx`: "the filter bar sends a structured filter...", "apply_shows_preview_then_reloads_on_commit" |
| conflict, then Reload row | `ui/src/tabs/DataView.test.tsx`: "conflict_keeps_stage_and_marks_row", "Reload row drops that row's staged change and reloads" |
| table without a key shows a read-only banner | `ui/src/tabs/DataView.test.tsx`: "says why a table is read-only and edits nothing", "maps the other reasons" |
| `unknown` outcome shows a banner | `ui/src/tabs/DataView.test.tsx`: "unknown_outcome_banner" |
| Production + DROP needs the typed name | `ui/src/dialogs/RunConfirmDialog.typed.test.tsx`: "typed_name_gates_run_button", "explains_a_drop"; `ui/src/store/runQuery.confirm.test.ts`: "sends_the_typed_names_to_query_run"; and the real app in `e2e/query.e2e.ts` step 9 |
| Manual commit, then Rollback | `ui/src/components/StatusBar.test.tsx`: "shows_transaction_states_with_buttons" (Commit click sends `query_tx` Commit; Rollback button shown); `ui/src/store/queryTabs.test.ts`: "close_tab_with_tx_asks_commit_rollback_cancel" (Rollback sends `query_tx` Rollback); `ui/src/dialogs/TxCloseDialog.test.tsx`: "Rollback settles with Rollback"; `ui/src/components/StatusBar.tx.test.tsx`: "a_lost_transaction_asks_for_a_rollback_and_never_says_rolled_back_when_unknown" |

Gap: no test clicks the status-bar Rollback button of an ordinary (not lost) open manual transaction and asserts the
`query_tx` call. Rollback is proven through the close-tab dialog and the lost-transaction button instead.

## 3. Spec §5.6 against tests

Paths are relative to `D:\dev\dnt-p3`. "6 servers" means `Target::all()` (MySQL 5.7, 8.0, MariaDB 10.6, 11.4, PostgreSQL 12,
17) in the `DNT_TEST_DB=1` run.

| §5.6 bullet | Proving tests | Status |
|---|---|---|
| On all 6 servers: filter, edit, insert, delete on a table with a primary key | Filter and sort on every target: `src-tauri/tests/export_filter_db.rs` `filters_sort_and_count_on_every_target`. Update, insert, delete: `src-tauri/tests/data_edit_db.rs` `update_insert_delete_round_trip` (loops `Target::all()`); `inserted_row_is_read_back`. Real app, MySQL 8 and PG 17: `e2e/data-edit.e2e.ts` | Proven |
| Table without a PK is read-only with a reason | `crates/query/src/edit.rs` `no_key_table_is_read_only`, `editable_uses_pk_then_not_null_unique`; `src-tauri/tests/table_data_db.rs` `data_editable_reports_reasons`; UI `DataView.test.tsx` "says why a table is read-only and edits nothing" | Proven |
| Conflict detected for UPDATE and DELETE; PG by `xmin`, MySQL by `Snapshot`; protection level shown | `src-tauri/tests/data_edit_db.rs` `concurrent_update_is_a_conflict` (UPDATE), `concurrent_change_then_delete_is_a_conflict` (DELETE), both over `Target::all()`; `case_only_change_is_a_conflict_on_ci_column`, `update_to_the_same_value_is_not_a_conflict`; `crates/query/src/edit.rs` `delete_uses_full_snapshot_not_only_edited_columns`; level shown in the preview: `DataView.test.tsx` "warns about a table that cannot roll back and about unchecked columns" | Proven. Two sessions on one row is tested at the driver level, not in the UI |
| MyISAM: Apply one statement at a time, preview warns it cannot roll back; Production blocks editing through the Data tab | `src-tauri/tests/data_edit_db.rs` `myisam_apply_is_one_statement_and_partial_is_reported`; `src-tauri/tests/data_edit.rs` `non_atomic_plan_has_one_statement`; `crates/query/src/edit.rs` `myisam_is_not_atomic`, `production_blocks_non_atomic_edits`; UI "warns about a table that cannot roll back and about unchecked columns" and "a table that cannot roll back stages one change at a time" | Proven. The Production block is a unit test in `dnt-query`, with `production_needs_confirmation` for the command; no DB test runs a MyISAM edit on a Production-tagged connection |
| Preview, then change `sql_mode` / table structure / reconnect: Apply returns `PlanStale`, nothing runs | `src-tauri/tests/data_edit.rs` `sql_mode_change_makes_plan_stale`, `fingerprint_change_makes_plan_stale` (structure), `dialect_change_makes_plan_stale`, `tunnel_or_connection_change_makes_plan_stale`, `non_atomic_reconnect_during_checks_is_stale`, `tunnel_replaced_during_apply_checks_is_stale`; UI "plan_stale_repreviews" | Proven (mock driver in `data_edit.rs`) |
| Simulated disconnect during `COMMIT`: UI shows "outcome unknown", no retry | `src-tauri/tests/data_edit.rs` `connection_lost_during_commit_is_unknown`, `connection_lost_before_commit_is_rolled_back`, `unknown_outcome_is_stored`; `src-tauri/tests/query_sessions.rs` `commit_lost_reports_outcome_unknown`, `connection_lost_on_commit_is_an_unknown_outcome`; UI "unknown_outcome_banner" (the stage is kept, no second apply call) | Proven |
| No path produces a write statement from a cut cell (type-level: no `From<Value> for FullValue` for `Bytes`) | Behaviour: `crates/core/src/value.rs` `truncated_bytes_are_not_full`, `full_bytes_display_as_preview`, `long_text_may_be_page_truncated`; `crates/query/src/edit.rs` `key_that_may_be_truncated_is_rejected`, `float_and_truncated_cells_are_unchecked`; UI "value_editor_stages_a_cut_cell_with_the_token_of_its_full_value" | Partly proven. `FullValue` has only the fallible `try_from_value` and no `From<Value>`, but **no test asserts the absence of that impl** (no `compile_fail` doctest or trybuild test). The guarantee is by construction and by the behaviour tests above |
| No write runs without passing the guard; `DELETE FROM t` without WHERE on Production needs the table name typed | `crates/query/src/guard.rs` `production_unbounded_requires_typed_table`, `drop_matrix_by_env`; `src-tauri/tests/query_sessions.rs` `run_without_confirmation_is_refused_on_production`; `src-tauri/tests/data_edit.rs` `production_needs_confirmation`, `read_only_connection_blocked_even_if_ui_sends_plan`; UI `RunConfirmDialog.typed.test.tsx` "typed_name_gates_run_button" | Proven for the listed paths. "Every write passes the guard" is a design property; no single test enumerates all write commands |
| Literal round trip for every supported type, including `NO_BACKSLASH_ESCAPES` | `crates/core/tests/literal_props.rs` `literal_never_escapes_its_quotes`, `binary_literal_is_hex_only`; `crates/core/src/sql/build.rs` `mysql_escapes_backslash_and_quote` and its `no_backslash_escapes` cases; DB round trip: `crates/driver-mysql/tests/execute.rs` `literal_round_trip_all_types` (runs with the default `sql_mode` and with `NO_BACKSLASH_ESCAPES`) and `no_backslash_escapes_tracks_flag`; `crates/driver-pg/tests/execute.rs` `literal_round_trip_all_types` | Proven |
| Manual commit: commit and rollback correct; warning for DDL that commits implicitly on MySQL; warning for a long-open transaction | Commit/rollback: `src-tauri/tests/query_sessions.rs` `query_tx_rollback_returns_idle`, `manual_mode_begins_before_first_write`, `manual_commit_mid_run_begins_again`; implicit commit: `mysql_ddl_in_manual_tx_is_flagged_implicit_commit`, `manual_mysql_ddl_commits_and_the_next_write_begins_again`, UI `RunConfirmDialog.typed.test.tsx` "implicit_commit_warning"; long-open: UI `StatusBar.tx.test.tsx` "status_bar_shows_open_tx_age_and_warns", "uses_the_configured_threshold" | Proven. The age warning is a UI-timer test; no Rust test covers the 5-minute threshold because it lives in the UI |

Bullets with no proving test: none. Bullets with a stated gap: the type-level `FullValue` bullet (no compile-time or
trybuild test of the missing `From` impl), and the MyISAM-on-Production bullet (unit-level only).

## 4. Known limits of this run

- No browser-level E2E (`e2e-web`) for P3 until it lands on `main`; the Vitest tests above stand in (ruling R1).
- The smoke scenario drives the Data tab through the Shift+Enter value editor, not the in-canvas cell editor.
- Only MySQL 8.0 and PostgreSQL 17 are covered by the desktop smoke; the other four servers are covered by the DB suite.
- `§5.2 P3b` (editing in the query result grid) is outside this plan.
