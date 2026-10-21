# Backend result spooling and export implementation plan

**Spec:** `docs/superpowers/specs/2026-10-21-backend-result-spooling-and-export-design.md`

## 1. Result-store core

- Add an ephemeral, process-owned result directory and stale-directory cleanup.
- Add framed exact row storage, committed frame indexes, lifecycle ownership by connection/tab/run,
  page projection, exact selected-value reads and bounded search/sample helpers.
- Add unit tests for exact values, frame boundaries, clipping, stale tokens and cleanup.

## 2. Query streaming integration

- Give the channel sink the run identity and result store.
- Persist complete preview values before announcing committed row ranges.
- Add bounded first-page/page-fetch events and commands; regenerate bindings.
- Detach old results on rerun, tab close and connection close.

## 3. Sparse query grid

- Replace append-only complete chunks with sparse display pages and committed row counts.
- Load visible pages with prefetch and bounded LRU retention.
- Route copy, find, record view and width samples through exact/backend result APIs where required.
- Preserve selection, scrolling, result tabs and Messages behavior.

## 4. Exact stored-result export

- Export a query result from its store token instead of rerunning SQL.
- Move all streaming format/file work to a dedicated bounded blocking writer.
- Preserve progress, cancellation, temporary-file cleanup and error precedence.

## 5. Obsolete connection-open cancellation and row-limit copy

- Add cancellation notification to the data request opening phase.
- Race connect against cancellation and recheck before SQL; retain unlimited independent concurrency.
- Rename user-facing row-limit text/documentation to display-row limit.

## 6. Verification

- Run focused Rust and UI tests after each integration boundary.
- Run formatting, Clippy, frontend lint/build and complete relevant suites.
- Run the six-engine DB tests and desktop smoke flows locally; do not add CI.
- Record final behavior, measurements and any remaining limitations.
