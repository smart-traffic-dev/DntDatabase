# Backend result spooling and export design

**Date:** 2026-10-21  
**Status:** Approved in conversation; awaiting review of this written specification  
**Scope:** Query-result memory, exact copy/export, obsolete data-session opens, row-limit semantics, and export I/O isolation

## Goals

1. A query result must remain exact for copy and export. The backend must never silently replace, cut, or omit a value that those actions consume.
2. Large results must not require the frontend to retain every complete value, and Tauri row events must remain bounded.
3. Independent user operations must remain concurrent. The application will not impose a new active database-connection limit.
4. A cancelled or superseded request must stop waiting for or opening a data session as soon as practical.
5. File formatting and writes during streaming export must not monopolize Tokio worker threads.
6. The existing transaction, guard, cancellation, history, and outcome-unknown rules remain unchanged.

## Non-goals

- Rewriting user SQL to append `LIMIT`.
- Cancelling or dropping a database session merely because the display row limit was reached.
- Introducing a global or per-connection concurrency limit.
- Persisting query results across application restarts.
- Removing the existing user-selected row limit of 1 to 1,000,000 rows per result set.

## 1. Disk-backed exact query results

### 1.1 Result store

`SessionManager` owns a process-local `ResultStore`. Each accepted query run gets a unique store entry keyed by `(run_id, result_id)`. The store writes complete `Value` rows, including complete binary values, into files in an application-owned temporary result directory.

The on-disk representation is an internal implementation detail. It must provide:

- append-only batches while the result is streaming;
- an index from row ranges to file frames so a page can be read without decoding the whole result;
- atomic publication of each frame: readers never observe a partial frame;
- exact round-tripping of every `Value` variant;
- committed row count and completion/error state;
- cleanup without depending on a successful query finish.

The store uses a dedicated blocking worker. Database row callbacks hand batches to that worker through a bounded queue. Waiting for queue space is wrapped with Tokio's blocking-region support so local disk backpressure does not pin an async executor worker. There is no lossy fallback. A file or serialization failure stops the statement through the existing run cancellation path and reports an explicit error.

The result directory is created beneath the app data directory. Startup removes stale result directories left by crashed earlier processes. The live directory is deleted on normal shutdown.

### 1.2 Display pages

Complete values are stored before a row range is announced to the UI. Run events no longer carry an unbounded stream of complete rows. They announce committed row ranges and may include one bounded display page for low-latency first paint.

The UI requests display pages by `(run_id, result_id, offset, limit)`. A page:

- contains at most 500 rows;
- limits its approximate cell payload to 4 MiB, excluding serialization overhead;
- uses a 64 KiB display preview for an individual text, JSON, other-text, or binary cell;
- marks incomplete display cells explicitly, including original byte length where available;
- never changes the exact value in the result store.

If one row's individually capped cells would exceed the page budget, the page builder reduces
their preview lengths further so it can still return that row with every column present. It never
drops a column or substitutes an exact value in copy/export with the reduced preview.

The query grid keeps a sparse LRU page cache with a 32 MiB approximate payload budget per tab. Visible pages and a small prefetch window are retained; old off-screen pages may be evicted and fetched again. Row count, selection coordinates and scroll position are independent of whether a page is currently resident.

The first page remains visible while a query is running. As committed row ranges arrive, the grid row count grows. Scrolling to a committed range loads it without waiting for the whole statement to finish. A range not committed yet shows the existing loading treatment rather than an empty value.

### 1.3 Exact copy and existing grid tools

Copy obtains the selected rows and columns from the backend result store, not from display previews. The current 100,000-cell copy safety limit remains. The backend returns exact `Value` objects for the selected block and the existing frontend formatters continue to produce TSV, CSV, headers and SQL INSERT text. The transient allocation required by an explicit copy is allowed; ordinary viewing remains bounded.

Find-in-results scans the result store so evicted pages are included. Navigation loads the page containing the match. Column sizing may sample committed rows from the store, with a bounded sample, rather than requiring all rows in frontend memory. Record view loads the exact selected row from the store.

If a result file is unavailable or corrupt, copy/find/page commands return a clear result-expired/internal error. They never fall back to a clipped display value.

### 1.4 Export consistency

Exporting a result that still has a valid result-store entry reads the exact stored rows. It therefore exports the same result the user saw and does not rerun the query. This applies to completed and row-limit-truncated results.

An export requested while the result is still running reads committed frames and then follows new frames until completion, cancellation or error. The export remains cancellable. If the store entry has expired, the UI reports that the result is no longer available; it does not silently rerun SQL whose data or session state may have changed.

Table export remains a fresh table read because it is not tied to a query result.

### 1.5 Lifetime and cleanup

A result entry remains valid until the earliest of:

- the same tab starts a replacement query run;
- the tab is closed;
- its connection is closed;
- the application exits.

Starting a new run detaches old entries immediately from commands, then removes their files asynchronously. Closing a connection removes all entries belonging to it. Cleanup failures are logged and retried at next startup.

There is no arbitrary result-size quota beyond the existing row limit and available disk space. If the disk cannot accept more data, the run fails explicitly and preserves normal transaction/outcome reporting.

## 2. Data-session concurrency without a connection cap

The idle pool remains capped at two sessions per database, but active operations remain unlimited.

The opening phase gains its own cancellation notification. When a request key is cancelled or superseded:

1. a connect attempt that has not started is skipped;
2. an in-progress connect future is dropped through a cancellation-aware `select`;
3. a connect that wins the race is checked again before the session can run SQL;
4. any session produced for an obsolete request is closed or safely returned to the idle pool without executing the request.

Requests with different keys continue opening and running concurrently. Edit applies, exports and independent table/query operations receive no new semaphore or queue.

## 3. Row-limit semantics

The toolbar value is named and described as a **display row limit**. Reaching it means no additional rows are retained in the result store or shown for that result. Both drivers may still drain remaining protocol rows to keep the session valid, so database execution time can exceed the time needed to produce the displayed rows.

The backend and UI documentation state this behavior. The result remains marked `truncated` when more rows existed. No automatic SQL rewrite, early session cancellation, or connection abandonment is introduced because those actions can change user-function side effects, multi-result behavior, transaction state, and outcome reporting.

A future protocol-specific cursor design may add a separately named execution/fetch limit. It is outside this change.

## 4. Streaming export worker

Formatting and file writes move to a dedicated blocking writer worker. The database-facing `RowSink` sends owned row batches through a bounded channel. Queue capacity is governed by approximate payload bytes, with a 32 MiB maximum, rather than only batch count.

When the queue is full, sending uses a blocking region so backpressure slows the producing database stream without occupying a Tokio worker. The worker owns `FormatWriter`, the temporary file and all synchronous format state. It sends progress, completion and the first write error back to the async command.

On write/format failure:

- the existing query/data cancellation path is invoked;
- queued rows are discarded;
- the worker closes the file;
- the `.dntpart` file is removed;
- the original write error wins over a later cancellation error.

On success, the worker finishes the format and atomically publishes the temporary file exactly as today. Loaded-row export also uses the blocking path.

## 5. Error and cancellation behavior

- User Stop keeps its current meaning and transaction reporting.
- Result-store and export-worker failures are ordinary explicit run/export failures; no partial result is presented as complete.
- A run whose SQL outcome may be unknown preserves `outcome_unknown` independently of a cache/write failure.
- Closing a tab or connection cancels the active run first, detaches its result entries, and lets blocking workers finish cleanup without delaying UI teardown indefinitely.
- Copy/page/find commands validate ownership by the tab's current run token so stale UI state cannot read a replacement run.

## 6. Compatibility and migration

The application database schema does not change. Result files are ephemeral and versioned internally so incompatible stale files can be deleted rather than migrated.

Generated TypeScript bindings change for result-range events and new result page/copy/find commands. Existing query history continues to store SQL and outcomes only; it does not retain result data.

## 7. Verification

### Unit and mock tests

- Exact `Value` round-trip for text, JSON, arrays, other types and complete binary values.
- Frame indexing, partial-write rejection, paging across frame boundaries and stale-token rejection.
- Display page cell/event limits and LRU eviction without data loss.
- Exact copy after the display page was clipped or evicted.
- Find and record view over an evicted page.
- Rerun, tab close, connection close and startup stale-file cleanup.
- Cancel before connect, during connect and immediately after connect; no SQL executes for the obsolete request.
- Distinct request keys remain fully concurrent.
- Export queue backpressure, writer failure, cancellation and atomic publication.

### Database-backed tests

- PostgreSQL and MySQL-family result containing text and binary values larger than display limits: grid preview is bounded; copy and export are byte-exact.
- Large multi-page result can scroll and copy from an evicted page.
- Manual transaction state survives display-row truncation exactly as before.
- Stop and connection close clean result files and do not regress outcome reporting.
- Table and query exports remain cancellable across the local six-engine matrix where supported.

### Performance checks

- A 500-row result with 128 KiB text cells keeps frontend event payloads at or below 4 MiB and the tab's resident page cache near 32 MiB while exact copy still succeeds.
- Twenty-four independent data reads remain concurrent and may open twenty-four data sessions.
- Twenty-four superseded opens execute only the surviving request.
- A deliberately slow export writer does not stall an unrelated Tokio timer/query; queued payload remains at or below 32 MiB.

## 8. Delivery order

1. Build and test the result-store framing, lifecycle and exact page/copy APIs.
2. Integrate run streaming and sparse frontend pages, then migrate grid copy/find/record view.
3. Export query results from the store and isolate all export writers.
4. Add cancellation-aware data-session opening without adding a concurrency cap.
5. Update display-row-limit copy, generated bindings and end-to-end tests.

