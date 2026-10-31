# R1 — carry-overs for R2 and later

R1 (`docs/superpowers/plans/2026-10-31-r1-driver-core-split.md`) is merged-ready; these items were found during its
reviews and deliberately left out of R1. Spec: `docs/superpowers/specs/2026-10-31-non-sql-driver-core-design.md`.

## Do early in R2

- Error wording for "no SQL on this connection" exists in three forms: `DbKind::require_sql` → `Unsupported
  { feature: "SQL on MongoDB" }`, spec's `dyn Session::sql()` → feature `"SQL"`, and
  `crates/schema/src/render/mod.rs:78` `RenderError::Invalid("SQL on X is not supported.")`. Unify.
- `Unsupported { version: "" }` renders as "… is not supported on server version " (pre-existing; also
  `check_cell_size`). Omit the clause when empty — changes an existing MySQL/PG message, so it was not done in R1.
- Add `SqlSession::sql_kind() -> SqlKind` (default from `dialect()`), so code holding a `&dyn SqlSession` stops
  re-deriving the kind with a fallible `server_info().kind.require_sql()?` (catalog.rs, ddl_edit/mod.rs,
  object_actions.rs, …) and dnt-schema's public fns (`parse_type`, `type_catalog`, `new_table`, `from_detail`,
  `fidelity`, `validate`) can become infallible again by taking `SqlKind`.
- ~21 `.sql()?` early returns sit between `open_query_session` and `close_quietly` (backup, import, seed,
  ddl_edit, table_design, object_actions): a failure drops the session instead of closing it. Unreachable while
  every entry point is gated; add an `open_sql_session` helper (or close-on-Err) before a Command session exists.
- "No `SqlSession` ⇒ Idle / not in a transaction" defaults (query_sessions `tx_open`, `tx_info`,
  `check_ddl_idle`, `tx_now`; data_sessions reuse checks) must be revisited when `CommandSession::tx_state` lands.
- Once `FakeCommandDriver` exists: one table-driven test running every gated `_impl` against a Command connection
  and against a SQL connection with each `Cap` off (today only Designer and DataEdit-cell-load are tested), plus
  tests for `query_run_named` → `Unsupported`, Mongo capabilities, and the Mongo/Redis arms of `home`,
  `metadata_key`, `home_database`, `load_schema_cache`, completion kinds.
- Gates live in two layers (`commands/*_impl` vs `SessionManager` methods); some manager methods re-check
  `require_sql` after an `_impl` gate. Pick one layer.
- The plan's gating table names commands that take no connection id (`query_run`, `query_tx`, `query_tx_mode`,
  `query_plan_statement`, `statement_ranges`, `import_auto_map`, `import_check_mapping`,
  `table_design_render/preview`, `ddl_edit_preview`); they are correctly ungated — do not "fix" them.

## R4 (UI)

- `sqlEngine()` falls back to `'mysql'` in `ColumnInspector.tsx:89` / `ColumnsGrid.tsx:156`; `newQuery.ts:64`
  `engineOf` builds MySQL text for unknown kinds. Remove with the `ui/src/engine/` module + ESLint rule.

## Separate follow-ups (not R2)

- **MySQL double-cancel race (real, pre-existing):** `crates/driver-mysql/tests/execute.rs`
  `cancel_twice_keeps_session_usable` failed once in a full DB run — a stray `KILL QUERY` reached the next
  statement (`SELECT SLEEP(0.5)` returned 1), i.e. a double-clicked Cancel can interrupt the user's next
  statement. Two PG cancel tests (`execute.rs:547`, `:1259`) were also intermittent. R1 did not touch the
  canceller/execute code.
- `commands::tests::guard_converts_panic_to_internal` failed in some parallel runs (log-capture race suspected).
- Pre-existing e2e-web tsc error: `e2e-web/specs/data-edit.spec.ts:24` (`env` not on `ConnectionConfig`).
- Cosmetic: under-indented returns in `ddl_edit/catalog.rs:128/155/190`; ~200-char one-line kind match in
  `object_actions.rs:241`; duplicated fixture-reader block (`schema/fidelity.rs`, `table_design/tests.rs`).
- `kind_branches` heuristics: a `matches!`/`==` wrapped across lines, or a blank/comment line between
  `#[cfg(test)]` and `mod`, is not recognised (none today).
