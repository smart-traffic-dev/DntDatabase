# Backend, core correctness and performance review

Date: 2026-10-21. Reviewed feature branch `feat/p4a-object-actions` at f92d0ab and main at eb49dc0. Main has no changes under crates/ or src-tauri/ since 7f5e5c9; the additional backend code on the feature branch is P4a. Main working changes were left untouched.

## Findings

### P1: Query preview has no byte budget

Locations: src-tauri/src/query_sessions.rs:2430; src-tauri/src/run_events.rs:185; crates/driver-pg/src/session.rs:432; crates/driver-pg/src/values.rs:174. MySQL preview has the same absence of a byte budget.

Query execution caps row count (up to 1,000,000), batches by 500 rows, and keeps full text values. RowStream also batches by row count and emits synchronously under its buffer lock. Unlike Table Data's 64 KiB cell / 32 MiB page limits, normal query preview does not bound cells, batches or total result bytes. Large TEXT/JSON results can cause very large allocations, serialization and IPC payloads, and frontend retention. A single large cell can already be expensive before a downstream limit could inspect it.

Measured with the actual PostgreSQL 17 driver: SELECT repeat('x', 131072) FROM generate_series(1, 500), max_rows=500, batch_size=500, default Preview mode retained 65,536,000 bytes (62.5 MiB) of text. This is payload size, not process RSS or an end-to-end UI benchmark. Actual Tauri serialization overhead was not measured.

Recommendation: introduce explicit per-cell, per-event and total preview byte limits, expose truncation, and retain a separate full-value/export path. Account for driver decoding allocations as well as the event sink; simply capping after a 500-row driver batch is insufficient for peak memory.

### P1: Active data connections are unbounded

Locations: src-tauri/src/data_sessions.rs:48 and :550. IDLE_PER_DATABASE=2 only limits returned idle connections. data_exec opens another physical session whenever none is idle; there is no active-connection semaphore. Distinct request keys can all open sessions concurrently. Superseded requests are checked after opening, so rapid refreshes while connecting can also amplify connection churn.

Measured using the existing fake-driver harness: 24 simultaneous SLOWISH reads with distinct keys opened 25 sessions including the metadata session (24 active data sessions). This proves the application-level behavior, not a measured database connection-limit failure.

Impact: concurrent tabs/loads/exports can consume the server's connection allowance and multiply TLS/SSH/setup costs. It is a single-user desktop app, but connection burst size is currently determined by UI concurrency rather than a defined resource budget.

Recommendation: a per-connection active-session budget with cancellable acquisition and a bounded queue. Ensure Data reads, edits and exports share an intentional budget; preserve transaction affinity and reserve control/cancel capacity. Bound connection opening itself and check cancellation before opening and after acquiring capacity.

### P2: max_rows stops delivery but drains the full result

Locations: crates/driver-pg/src/session.rs:422 and crates/driver-mysql/src/session.rs:829.

Both drivers continue reading and discarding rows after the requested result limit. With a raw SELECT without SQL LIMIT, the UI limit does not bound server execution, result transfer or completion time. A query can deliver its visible rows quickly and then remain running until the much larger result finishes or times out.

Measured on local PostgreSQL 17 with max_rows=1:
- SELECT i, pg_sleep(0.003) FROM generate_series(1, 200) i: 805-2,102 ms across two runs, one delivered row.
- The same SQL with LIMIT 1: 6 ms, one delivered row.

These are diagnostic observations from two local runs, not stable benchmark thresholds. The second ran alongside DB correctness checks; load and scheduling were not controlled. PostgreSQL sleep/timer resolution contributes to the absolute values. The code also confirms the MySQL drain behavior; equivalent MySQL timings were not measured.

Recommendation: explicitly define whether the UI option means display cap or execution fetch cap. If a true fetch cap is required, evaluate PostgreSQL cursor/portal fetching and protocol-safe MySQL termination/replacement. Preserve transaction state and side-effect semantics. Do not append LIMIT to arbitrary user SQL or indiscriminately cancel writes / side-effecting SELECTs.

### P2: Streaming export performs blocking file work on async workers

Locations: src-tauri/src/export_format.rs:310 and :1014; src-tauri/src/commands/export.rs:163.

ExportSink.rows synchronously formats each row and calls the file-backed writer on the database async execution path. CSV/JSON/SQL writes use synchronous BufWriter::write_all; XLSX row formatting/writes also run there. Final finish/commit is moved to blocking(), but the streaming row path is not.

A slow destination or enough simultaneous exports can occupy Tokio workers, increasing latency for queries and cancellation. Exact responsiveness degradation has not been benchmarked; this finding is based on the concrete blocking call path, not measured UI freezes.

Recommendation: run the writer on a blocking worker and feed it via a bounded channel with cancellation and backpressure. Propagate disk errors, stop the producer, close handles and preserve current temporary-file cleanup / atomic publication behavior. Include both row-count and byte limits on queued work.

## Core correctness assessment

The focused tests cover guarded one-use plans, policy rechecks, stale metadata/dialect/generation, optimistic edit conflicts, transaction rollback, commit loss/unknown outcomes, late-cancel isolation and session reuse. No new correctness defect was established in those tested paths during this review. This is not a proof against all data-loss scenarios.

## Reproducible local probes

Two ignored diagnostic tests are retained beside the existing harnesses. They report observations rather than enforce a performance SLA. Use CARGO_HOME=D:/dev/cargo, RUSTUP_HOME=D:/dev/rustup, and DNT_TEST_DB=1 for the PostgreSQL probe:

```text
cargo test -p dntdatabase --test data_sessions review_probe --offline -- --ignored --nocapture
cargo test -p dntdatabase --test object_actions_db review_probe --offline -- --ignored --nocapture
```

The DB probe uses SELECT only and creates no schema/table. No production behavior or CI configuration was changed.

## Validation

- Focused query_sessions, data_edit and data_sessions run: 135 existing core tests plus the initial concurrency diagnostic passed (136 total before marking probes ignored).
- Both performance diagnostics passed; PostgreSQL diagnostic explicitly ran with DNT_TEST_DB=1.
- DB-backed core checks with DNT_TEST_DB=1: data_edit_db 17/17, export_filter_db 9/9, query_db 8/8; 34 total passed. These cover the local engine matrix where each test supports it, including edit conflicts/full cells, filter/sort/export round trips, read-only enforcement, Stop and manual rollback.
- cargo fmt --all --check, git diff --check and cargo clippy -p dntdatabase --tests --offline -j2 -- -D warnings passed.
- After marking the probes ignored, both were explicitly rerun with --ignored --nocapture and passed.

## Priority

Address preview memory budgets and active connection budgets first. Then clarify/fix fetch-limit behavior and isolate the export writer from async workers. Defer speculative cache/index tuning until representative schema sizes and query/export workloads are measured.
