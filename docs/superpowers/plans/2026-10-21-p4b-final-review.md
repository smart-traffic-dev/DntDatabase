# P4b table designer — final review

- Date: 2026-10-21
- Branch: `feat/p4b-table-designer` (from `feat/p4a-object-actions` 6a7c8fd), spec/plan 107624e / 3150bb4, head after final fixes bfce09f.
- Process: subagent-driven development, 16 implementation tasks, each with a task review and fix rounds; one whole-branch review on Opus plus one fix wave and its re-review. Final verdict: no Critical/Important open.
- Merge order: only after P4a is merged (P4b reuses its DDL plan/operation infrastructure, extracted into `src-tauri/src/ddl_exec.rs`).

## Environment and verification

Windows 10, PowerShell 5.1, Docker matrix `testdata/docker-compose.test.yml` (MySQL 5.7 / 8.0 / 8.4, MariaDB 10.6 / 11.4, PostgreSQL 12 / 17).

| Command | Result |
|---|---|
| `cargo fmt --all --check` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `DNT_TEST_DB=1 cargo test --workspace -- --test-threads=1` (at c53e6da) | all pass except 2, both unrelated to this branch: `dnt-driver-mysql` `late_result_with_unconfirmed_kill_is_kept` (timing flake, passed 3/3 reruns, no driver execute code changed) and testkit `no_bom` (pre-existing BOM in `ui/src/monaco/fonts.test.tsx`, `ui/src/theme/tokenCompliance.test.ts`) |
| After final fixes: `cargo test -p dntdatabase -p dnt-schema`, `DNT_TEST_DB=1` `--lib table_design` (27) and `--test table_design_db` (7, all 7 servers) | pass |
| `pnpm --dir ui lint` / `pnpm --dir ui test` | clean / 1502 pass |
| `pnpm e2e:web` | 18 pass |

Not done: desktop (Tauri) smoke on MySQL 8 and PostgreSQL 17 with light/dark screenshots against board 10. The designer was exercised in the browser via e2e-web and a temporary Playwright screenshot during Task 13 only.

## Decisions taken during implementation (rulings)

Each was decided against the spec; "cost" is what rework looks like if the decision is wrong.

1. P1 — `IndexDef.constraint` is produced by the model task, not added later by the PG render task. Cost: none.
2. P2 — New table opens as `new_table` with one `id` column; the user renames it. Cost: cosmetic.
3. P3 — The unsaved-changes chip counts added/removed/modified top-level elements by `original`. Cost: UI count only.
4. T3-1 / T5-1 — FK `MATCH SIMPLE` (default) is `None`; `FkDef.match_type` carries FULL/PARTIAL. Cost: one-line change.
5. T5-2 — Column charset/collation equal to the table's are dropped from the model. Cost: explicit redundant spelling is lost (no semantic change).
6. T5-3 — User fragments on MySQL must be safe under all four `ANSI_QUOTES`×`NO_BACKSLASH_ESCAPES` modes. Cost: rare valid fragments refused.
7. T5-4 / T5-7 / T5-9 — Client `locked` flags are never trusted; the backend restores locked elements from its stored base and rejects unknown `original`s. Cost: none.
8. T5-5 — MariaDB IGNORED indexes are locked. Cost: not editable.
9. T5-6 (+EXCLUDE, T9-1 tablespace) — Unmodelled index attributes, NOT ENFORCED checks, EXCLUDE constraints and non-default tablespaces lock their element. Cost: those elements not editable.
10. T5-8 — The P4a identifier rule (no dots, length) applies only to names the user creates or changes. Cost: none.
11. T5-10 / T11-3 — MySQL 8 expression defaults and checks with escaped string literals are locked. Cost: not editable (follow-up: unescape in the driver).
12. T6-1 — Rename+alter+move of one MySQL column merge into one `CHANGE/MODIFY … AFTER`; PostgreSQL has no column reorder. Cost: none.
13. T6-2 — Name swaps/chains/reuse inside one edit are refused ("apply in two steps"). Cost: two applies for such edits.
14. T6-3 — PostgreSQL statement order follows the spec regardless of the internal change order. Cost: none.
15. T7-1 — Render takes the session `Dialect` so MySQL literals honour `sql_mode`. Cost: signature differs from the plan.
16. T7-2 — Changing an FK's content under the same name on MySQL is refused (rename it or apply twice), keeping one atomic `ALTER`. Cost: usability on FK edits.
17. T7-3 — MySQL value defaults are always quoted. Cost: none.
18. T8-1 — PostgreSQL literals use standard `'…'`; apply and counts run `SET LOCAL standard_conforming_strings = on`. Cost: none known.
19. T9-2 — `SHOW CREATE TABLE` is read with `ANSI_QUOTES` switched off for that read. Cost: none.
20. T10-1 — Impact counts are stopped server-side on timeout (driver KILL/cancel, PG `statement_timeout`, MySQL `max_execution_time`, MariaDB `max_statement_time`). Cost: none.
21. T11-1 — A PostgreSQL lock wait at apply's pre-send re-read arrives as a 55P03 error, shown as "not applied". Cost: none.
22. T11-2 — MySQL 1205 lock wait on a sent DDL maps to NotApplied in the shared `ddl_exec`; **this also changes P4a object actions** from Unknown to NotApplied (correct: the server changed nothing). Cost: visible P4a behaviour change.
23. T13-1 — Read-only connections/tables open the designer view-only (no drafting). Cost: no offline drafting.
24. T13-2 — Auto increment (MySQL), identity (PostgreSQL) and ON UPDATE (MySQL) editing were missing from the plan and were added to the column inspector. Cost: none.
25. Final review — After "Table changed", the previous draft is kept for comparison (read-only list + Dismiss, no blind restore); rebuild warnings show the row estimate; "Preview again" after an expired plan.

## Known follow-ups (not blocking)

- P4a object-action dialog ignores open design tabs: a design of a renamed/dropped table later fails with a server error (safe, nothing sent).
- If reopening fails after a successful apply, the designer keeps the pre-apply definition until reopened (an apply would then be refused).
- "Restore my changes" after a lost connection replays the whole lost draft rather than only the user's changes.
- Comma-separated column lists in FK/index editors (names containing commas), prefix input validation, PG constraint-backed index Unique still editable.
- MySQL 8 checks/defaults with string literals stay locked until the driver unescapes them.
- Designs never expire server-side (cap 64 per app run).
