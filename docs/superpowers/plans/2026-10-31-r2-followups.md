# R2 — hand-off for R3, R4 and the driver specs

R2 (`docs/superpowers/plans/2026-10-31-r2-command-family.md`) is done. These decisions and gaps came out of its reviews.
Spec: `docs/superpowers/specs/2026-10-31-non-sql-driver-core-design.md`.

## Spec amendments (the code already does this)

- §4.3 `RiskFacts` has `targets: Vec<CommandTarget>`. Its confirm-name rules need them: an `unbounded` command
  confirms with the first target's name. An `Unknown` command with a target confirms with that target's name on
  Production, otherwise with the connection name.
- §4.3 risk rules apply to any `Allow` result, as `guard::stmt_verdict` does. On a read-only connection, any command
  with drops or `unbounded` is Block, whatever its class.
- §4.1 db-relative targets: `CommandTarget.database == None` means "the db in effect when the command runs".
  A `Database` drop with an empty name means "the db in effect".
  - `plan_commands(text, target, lang, current_db)` resolves both through the planned commands only.
  - A `SessionChange` inside a transaction makes the db unknown until the end of the plan.
  - An unknown or nameless database drop confirms with the connection name.
- §4.1 `CommandDropKind::AllDatabases` (FLUSHALL-like): confirmed everywhere and on Production with the connection name.
- §4.1 `CommandSession::current_database()`: drivers report the db their connection is in and re-apply it on
  reconnect. Before a command whose targets were resolved from the db, `run_commands` stops with
  `StopReason::DatabaseChanged { index, expected, actual }` when the two differ.
- §4.4 `run_commands` refuses an unguarded plan. A failed `SessionChange` always stops the run, even with
  `continue_on_error`.
- §7.4 `DataAccess` returns `DbError`. `check_read` also requires no drops, no `unbounded` and no session effect.
  `KeyEdit` values, fields and members are `KeyBytes` (base64). `CellInput`, `RowRef`, `RowEdit` and `EditSet` live
  in `dnt_core::edit`.

## Driver specs (R3 / Mongo / Redis) must state

- Every drop names its target. Use `AllDatabases` for FLUSHALL.
- A KEYS/SCAN-like read must not be marked `unbounded`, or it is blocked on read-only connections.
- A bad db in the config is an error, not a silent fallback to db 0.
- An aborted transaction (Redis `EXECABORT`): `run_commands` has no `TxAborted` pre-check. Decide whether the
  driver reports it as an error.
- `KeyName.display` is not injective. Confirm names for binary keys are therefore ambiguous; identity is `raw`.

## R4 (app + UI)

- Take `current_db` for planning from `CommandSession::current_database()`, not from the tab's memory. A USE inside
  MULTI is not reported through `sink.database`.
- Project `CommandPlan` / `PlannedCommand` onto `PlanStmtView`. History, favorites and logs redact through
  `CommandLanguage::redact`. `PlannedCommand` and `CommandPlan` `Debug` print raw text.
- `query_tx_mode` (`query_sessions.rs:1417`) and `query_plan` (`:1117`) still use `.sql()?.tx_state()`; switch them to
  `dyn Session::tx_state()`.
- A tab keeps a refused Command session until the tab or connection closes.
- Key tabs carry `KeyName.raw` (base64), never `display`, as identity.
- The keyspace `u64`/`i64` fields need `#[specta(type = …)]` before they are exported.
- The R1 UI fallbacks (`sqlEngine()` → `'mysql'`) are still to be removed (see `2026-10-31-r1-followups.md`).

## Small follow-ups

- Tests: LateCompletion in `run_commands`; Collection/Key fed into the SQL refusals; `from_bytes` edge cases; leak
  checks around `object_action_preview` in `command_sessions.rs`.
- DRY: `plan_commands` target selection and `command_error_offset` duplicate `plan::plan` / `run::error_offset`.
- The fake's `redact` skips comment lines; a blocked-then-cancelled command is not in `executed()`.
