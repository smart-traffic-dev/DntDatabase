# P4c DDL edit — final review

- Date: 2026-10-25
- Branch: `feat/p4c-ddl-edit` (from `main` 2a55cbb). Spec/plan 62b2cb4; implementation 62b2cb4..498e134.
- Process: subagent-driven development, 10 implementation tasks, each with a task review (Task 1 needed three fix
  rounds on the MySQL body scanner), one whole-branch review on Opus, one fix wave (498e134) and its scoped
  re-review. Final verdict: ready to merge, no Critical/Important open.
- Models: every implementer and reviewer ran on Opus; the Sonnet model was not available in this environment
  (`model_not_found`).
- Merge: independent of P5/P6. Shared hotspots touched with small appended changes only (list below). No store
  migration.

## What was built

- `dnt_core::sql::ddl_edit` (pure): `isolate` (one statement; MySQL `DELIMITER`; exact block-matching body scanner
  for routines/triggers/events), `parse_header`, `check_target` (same kind, name, database/schema; version gaps
  as `Unsupported`), `build` (per engine/kind/strategy statements, PostgreSQL post-checks, warnings),
  `strategies`, `notes`, `template`.
- `src-tauri/src/ddl_edit/`: catalog facts, open/preview/apply/cancel/close, single-use plans with a fingerprint
  of the server DDL + catalog facts, guard per statement, typed confirmation (Production; Staging when the plan
  has a MySQL drop gap), PostgreSQL transaction with post-checks, MySQL sequential apply with re-read of the
  object after a failure (`NotApplied` / `Partial` + `Missing` + restore text / `Unknown`), MySQL routine
  rehearsal under a temporary name, History, tree/SchemaCache refresh.
- PostgreSQL tree gets a Triggers folder (each trigger name listed once).
- UI: `ddl` tab kind, `useDdlEdits` store, DDL editor tab (header, strategy switch, Monaco, Revert, banners),
  review & apply dialog, entry points (object tab DDL **Edit**, tree **Edit definition** / **New …**), close
  guards (tab, group, connection, import, quit).
- e2e-web: fake handlers (`e2e-web/src/ddlEdit.ts`) and `specs/ddl-edit.spec.ts` (6 flows).

## Environment and verification (head 498e134)

Windows 10, PowerShell 5.1, Docker matrix already running (MySQL 5.7 / 8.0, MariaDB 10.6 / 11.4, PostgreSQL
12 / 17; MySQL 8.4 not running and not started).

| Command | Result |
|---|---|
| `cargo fmt --all --check` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `cargo test --workspace --no-fail-fast` (no DB) | 1291 passed, 2 failed: `dntdatabase --test query_sessions` `events_follow_contract`, `partial_batches_are_coalesced`. Both fail deterministically and identically before any `src-tauri` change of this branch (they assert batch coalescing in `run_events`; no query/run code changed here). Not fixed: outside P4c. |
| `DNT_TEST_DB=1 cargo test -p dntdatabase --test ddl_edit_db -- --test-threads=1` | 10 passed; 34 per-target PASS lines (MySql57 6, MySql8 6, MariaDb106 5, MariaDb114 5, Pg12 6, Pg17 6); no `p4c_` leftovers |
| `DNT_TEST_DB=1 cargo test -p dnt-driver-pg --test introspect pg_trigger_names_listed_once` | 1 passed (Pg12, Pg17) |
| `bindings_are_current` | pass |
| `pnpm --dir ui lint` / `pnpm --dir ui test` | clean / 1587 passed |
| `pnpm --dir e2e-web typecheck` | clean |
| `E2E_WEB_PORT=5182 pnpm e2e:web` | 25 passed |

Not done: desktop (Tauri) smoke test and screenshots; canvas board 11 was not available locally, the UI follows
the designer's visual language. The SSH tunnel tests and other streams' DB tests were not run (by instruction).

## Rulings taken during implementation

Spec rulings R1–R14 are in the spec §10. Additional controller rulings:

1. History origin stays `object_action` (R1) — a `ddl_edit` origin needs a store migration, which this stream
   must not add.
2. The MySQL body scanner uses exact block matching (Begin / If / CaseStmt / CaseExpr / Loop / While / Repeat),
   openers only at statement starts, closers matched against the innermost block; the only name rule is "a word
   after `identifier.`". Unquoted identifiers named `begin` / `end` are refused (quote them). Three review rounds
   found heuristic variants bypassable (`ORDER BY begin`, `t.case`, `CASE WHEN end THEN begin`, `END END`,
   `1. END`).
3. `DbError::Unsupported` carries the user hint in its `feature` text (no new error field).
4. PostgreSQL apply sends `SET LOCAL standard_conforming_strings = on` after `BEGIN`; a PostgreSQL failure before
   any plan statement is `NotApplied` / `Unchanged`; `executed` / `failed` only for `Partial` (spec §6.2 updated).
5. The DDL tab title is the bare name / `New view`; the editor header shows `{Kind} · {name}`.
6. Edit entry points open view-only on read-only connections (blocked reason banner); the object tab's Edit is
   hidden while the connection is closed.
7. A MySQL apply whose every statement completed but whose session reconnected afterwards is `Applied` with a
   warning (final review).
8. An Applied report always carries `object`; the UI reopens the edit after a create or a failed re-read so the
   backend edit state stays authoritative.
9. History and Copy scripts pick a `DELIMITER` not contained in any statement.

## Shared hotspots changed

`src-tauri/src/lib.rs` (`pub mod ddl_edit;`), `src-tauri/src/commands/mod.rs` (`pub mod ddl_edit;` + 5 commands
appended), `src-tauri/src/session_manager.rs` (field `ddl_edits`, cancel on connection close, and the one-line
pre-existing clippy fix `is_none_or` at the `unnecessary_map_or` site), `ui/src/bindings.ts` (regenerated),
`e2e-web/src/backend.ts` (~15 lines: delegate non-table `list_objects` / `object_ddl` and the `ddl_edit_*`
commands to `ddlEdit.ts`), `ui/src/store/tabs.ts` (`DdlTabRef` in the `Tab` union, `closeConnection`),
`ui/src/store/dialogs.ts` (`ddlEdit` request, `ddlEditRunning`), `ui/src/store/queryTabs.ts`
(`setDdlDraftsSource`, quit waits while applying), `ui/src/store/treeModel.ts` (PG Trigger folder),
`ui/src/test/setup.ts` (Monaco mock exposes `data-read-only`), plus the `tab.kind` switches (EditorArea, TabBar,
TabStripActions, tabActions, StatusBar, HistoryPanel, resultExport, DialogHost), `ObjectTab.tsx`,
`TreeContextMenu.tsx`, `SqlPreview.tsx` (opt-in props), `table_design/mod.rs` (`ddl_not_shown` made
`pub(crate)`), `crates/core/src/sql/{mod.rs, multi.rs, split.rs}` (visibility only),
`crates/driver-pg/src/queries.rs` (`list_triggers` DISTINCT ON).

## Follow-ups (not blocking)

- **History origin `ddl_edit`:** add `HistoryOrigin::DdlEdit` ↔ `"ddl_edit"`, a migration rebuilding
  `query_history` with `CHECK (origin IN ('query','data_edit','object_action','table_design','ddl_edit'))`
  (number it after P5's migrations), History panel label "DDL edit", and set `ddl_edit::HISTORY_ORIGIN`.
- Two pre-existing `query_sessions` test failures (above).
- Warnings: MV refill shown for `WITH NO DATA`; trigger order warning not suppressed with `FOLLOWS`/`PRECEDES`
  (MariaDB order unverified); PostgreSQL drop + create owner change not warned; gap warning styled by text prefix
  (a structured flag would be safer); the `Unsupported` detail reads as the current version.
- MariaDB `FOR … END FOR` loops are refused by the scanner (safe false refusal).
- Template/build silent fallbacks for a missing schema / non-editable kind; `object_listed` case-insensitive on
  MySQL (Unknown instead of Missing in a rare case).
- UI polish: strategy radiogroup keyboard pattern; local `.btn:disabled` override; quitting asks twice when both
  designs and DDL tabs are dirty; StatusBar selector readability; dialog stage text after the run ends;
  duplicate warning keys.
- Tests: History assertions in the gap/stale DB tests, `Scratch::new` leak window, per-target panic collection,
  dialog not-closable-while-applying and trailing-space name tests, privilege filter vs `DISTINCT ON`.
- Rehearsal leftover warning may fire when Stop lands before the CREATE is sent (harmless, `IF EXISTS`).
- Desktop smoke test (MySQL 8 and PostgreSQL 17) and board 11 comparison.

## Process incidents

Subagents start in `D:\DntDatabase`; four times a relative path wrote into that checkout
(`ui/src/store/treeModel.test.ts`, `src-tauri/src/session_manager.rs`, `ui/src/components/TabBar.tsx` and two
test files, `e2e-web/src/backend.ts`). Each change was reverted (`git checkout --`) and `git status` there shows no
tracked changes. An untracked empty `D:\DntDatabase\layout.ts` of unknown origin was left in place.
