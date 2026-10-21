# Backend result spooling: implementation and verification

Date: 2026-10-21. Branch: `feat/p4a-object-actions` on top of `6a7c8fd`. Spec: `docs/superpowers/specs/2026-10-21-backend-result-spooling-and-export-design.md`. Plan: `docs/superpowers/plans/2026-10-21-backend-result-spooling-and-export-plan.md`.

## What changed

| Area | Behavior |
|---|---|
| Result store (`src-tauri/src/result_store.rs`) | Every query result with columns is written in full, binary values included, to an ephemeral per-process directory under the app data dir (`query-results/<uuid>`). Rows go through one bounded writer thread. A frame is indexed only after it is completely written. The encoding is binary, not JSON, so NaN and infinite floats survive. A frame whose row count does not match is rejected as corrupt. |
| Lifetime | Starting a new run, closing the tab or closing the connection detaches that run's results at once and deletes the files on the writer thread. On startup, a directory is removed when no live process holds its `owner.lock` and it is older than 60 s. |
| Run events | `result_stored {result_id, token}` and `rows_stored {result_id, start, row_count}` replace `rows` for query results. Explain still collects rows in memory. Small driver batches are still coalesced into ≤500-row batches before they are written. `result_end` comes only after every row is on disk. If a statement ends before its result does, the result is marked abandoned: its rows stay readable, but an export of it fails. |
| Commands | `query_result_page` (≤500 rows, 4 MiB page budget, 64 KiB text preview, 64-byte binary preview, truncated cells listed), `query_result_values` (exact, ≤100,000 cells), `query_result_find` (searches every committed row), `export_query_result` / `export_query_result_cancel`. |
| Grid | Keeps a sparse cache of display pages in an LRU within 32 MiB per result: the visible pages plus one page of prefetch on each side. Rows not loaded yet show the loading cell. A page that was partly filled when read is fetched again once more rows are committed into it. Copy (TSV/CSV/INSERT), Find and Record view read exact values from the backend. Selection statistics are skipped while part of the selection is not loaded. |
| Export | A query result is exported from its stored rows. The statement is not run again and the tab is not busy. The export can be cancelled while the result is still streaming. All streaming exports write on a dedicated writer thread through a queue bounded by payload bytes (32 MiB). A write error closes the queue, unblocks the producer, stops the statement and wins over a later cancel error. |
| Data sessions | A cancelled or superseded request drops its connect future (`select!` on the opening phase). A stale pooled session's retry is cancellable too. No connection cap was added. |
| Row limit | The toolbar says "Display limit N". `RunOptions::max_rows` documents that drivers may still drain the rest of the result. |

## Verification

| Check | Result |
|---|---|
| `cargo fmt --all`, `cargo clippy --workspace --all-targets -D warnings` | Passed |
| `cargo test --workspace --no-fail-fast` with `DNT_TEST_DB=1` (MySQL 5.7/8.0, MariaDB 10.6/11.4, PostgreSQL 12/17) | Everything passed except `dnt-testkit::no_bom`, which fails on committed main files `ui/src/monaco/fonts.test.tsx` and `ui/src/theme/tokenCompliance.test.ts` (BOM introduced by main `99d92c9`, unrelated) |
| New DB test `query_db::stored_results_are_exact_in_copy_and_export` | 140 KB text plus 140 KB binary on all six engines: the page is a bounded preview with both cells marked; copy values and the CSV export are byte-exact; closing the connection detaches the result |
| New unit tests | Codec round trip of every `Value` variant (NaN, -0.0, `i64::MIN`, `u64::MAX`) and rejection of a partial frame; rows spanning more than three frames; exact copy in any order; find; streaming that follows a running result; cancel; abandon; detach without failing the run; stale-directory cleanup that respects a live owner; export queue byte bound and write-error unblocking |
| UI lint, `tsc`, full Vitest | Passed (1,406 tests, plus LRU/page-refill and stored-event reducer tests) |
| Desktop E2E `query.e2e.ts`, WebView2 155 | Steps 1–8 pass on MySQL 8, including the new check that grid cells render from the stored pages. Step 9 (Stop on `SELECT SLEEP(30)`) fails with "run did not stop". A baseline build of committed `6a7c8fd`, without this change, fails at the same step, so it predates this work. Backend Stop is covered by `query_db::stop_sleep_under_one_second`, which passes. |

## Limits

- The display preview of a cut cell (overlay or record view before its exact load) shows the preview text. Copy, export and record view use exact values.
- Large single-row cells are read in full for `query_result_values`. This allocation is allowed by the spec for explicit copy.
- One writer thread serves all tabs: a slow disk slows every result being written at the same time.
- The native E2E Stop failure is pre-existing and still needs its own investigation.
