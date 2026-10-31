# P5 — Import / Export / Backup (transfer) — Design Spec

- Date: 2026-10-25
- Status: written by the P5 stream lead without a live review (the user is not available); every open
  decision is taken conservatively and recorded as a ruling (§2). Implementation follows the plan
  `docs/superpowers/plans/2026-10-25-p5-transfer.md`.
- Based on: framework spec `2026-10-20-core-roadmap-p3-p6-design.md` §2, §4, **§7** (N5, export, import,
  N6, dump/restore, N7, §7.7) and §11 Q5 (do both the integrated dump and external tools);
  P4b spec/plan as process model; spooling/export spec `2026-10-21-backend-result-spooling-and-export-design.md`.
- Branch: `feat/p5-transfer` from `main` at `2a55cbb`. Runs in parallel with P4c (DDL editing) and P6
  (admin, schema compare, ER). The store migration of P5 is **MIGRATION_7** (P6 takes 8).
- Sections: **P5a** jobs + streaming splitter (§4–§5), **P5b** import (§6), **P5c** dump / restore / external
  tools (§7). Testing and completion criteria in §8, dependencies in §9, follow-ups in §10.

## 1. Goals

1. Long transfers (import, dump, restore, exports) run as **jobs**: on their own session, with progress, Stop,
   a log, a persisted history, re-run with the same parameters, and they never block the UI.
2. Users import CSV/TSV, JSON/JSONL and XLSX into a table with column mapping, three modes and a **dry run that
   never touches the target table**.
3. Users back up a database with the integrated dump (no install needed) or with `mysqldump` /
   `mariadb-dump` / `pg_dump`, and restore through exactly three paths (§7.5 of the framework spec).
4. The P2/P3 promise holds: no write reaches a server without preview, plan and backend guard; read-only
   connections never receive a write; Production asks once per job, with a typed name; no automatic retry;
   `outcome_unknown` stays visible to the UI and History.

All UI copy is English.

## 2. Rulings

Each ruling is a decision taken against the framework spec where it was silent or ambiguous. "Cost" is what
rework looks like if the ruling is wrong.

| # | Topic | Ruling | Cost if wrong |
|---|---|---|---|
| R1 | Doc language | This spec and the plan are written in English (the framework spec is Vietnamese) so every implementer reads the same contract. | None |
| R2 | Job concurrency | At most **N running jobs per connection** (setting `jobs_per_connection`, default 2, range 1–8); extra jobs wait in state `queued`, FIFO per connection. No global cap. | Setting only |
| R3 | Existing exports | The three existing streaming exports (`export_table`, `export_query`, `export_query_result`) are **tracked** as jobs (they appear in the Jobs panel with progress and a working Stop and are persisted), but keep their current execution path, session and dialog. They are not re-runnable and do not count against the per-connection limit. Moving them onto job sessions is not clean: query exports must run on the tab's own session (same `sql_mode`, temp tables, transaction) by design (spooling spec §1.4). | Small: a tracked job can later become a real one |
| R4 | Progress transport | Tauri app event `job://progress` with a `JobEvent` payload, throttled to ≤ 5 events/s per job (state changes and the final event are never dropped). The UI also polls `job_list` every 2 s while a job is queued/running, so a missed event only delays the panel. | None |
| R5 | Re-run | "Run again" reopens the job's dialog/wizard pre-filled from the stored parameters. Write jobs (import, restore) always go through preview, plan and confirmation again; nothing re-runs without them. Read jobs (dump) start from the dialog's Start button. | UI only |
| R6 | Persisted job log | Memory keeps up to 2,000 log lines per job (first 1,000 + last 1,000, dropped lines counted). The database keeps the first 250 + last 250 lines, ≤ 256 KiB. Lines carrying a **server message** are persisted as `Server error <code>` only (as History never stores server messages, P2 §8.1); the full text is shown while the app runs. Lines never contain cell values the app generated (conversion errors name row, column and reason only). | Less detail after restart |
| R7 | Interrupted jobs | At startup, persisted jobs left `queued`/`running` (crash, kill) become `failed` with summary "The app closed while the job ran; its outcome is unknown." and `outcome_unknown = true`. Temporary job files live in a per-instance folder `<data_dir>/job-tmp/<instance>/` whose `owner.lock` is locked while that app instance runs; at startup the folders of instances that are gone (lock not held) are deleted. When another instance is running (its lock is held), no job is marked interrupted at that startup (its live jobs share the store); a later start marks them. | None |
| R8 | App close | Closing the app with queued/running jobs asks "N background jobs are running" → **Stop jobs and quit** / Cancel. Quit stops every job (waits ≤ 5 s) before the existing close flow. | UI only |
| R9 | Connection close | Closing a connection stops its jobs (like object actions); tracked exports are cancelled through their existing cancel path. | None |
| R10 | History for jobs | One History entry **per job** (not per statement) for import (`origin = import`) and restore (`origin = restore`), with a summary comment plus the statement templates; dumps write none (reads). MIGRATION_7 rebuilds `query_history` to allow the two origins. | Migration only |
| R11 | N6 contract | `StreamSplitter` with client detection **off** returns exactly `split`'s statements (text, start, end) for every input and every chunking. With detection on it differs only when the input contains `COPY … FROM stdin` or a line starting with `\` outside quoted text/comments. Input must be UTF-8 (an invalid sequence inside a statement is an error with its byte offset); a statement larger than 256 MiB is an error. | None |
| R12 | Import sources | CSV/TSV (UTF-8 only, BOM stripped, RFC 4180 quoting, delimiter `,` `;` tab `|`), JSON = a top-level **array of objects**, JSONL = one object per line, XLSX = one sheet (first by default). Arrays of arrays, other encodings and `.xls` are out of scope (§10). | New reader later |
| R13 | Column types for conversion | The logical type of each target column comes from the driver's metadata of `SELECT <cols> FROM <t> WHERE 1 = 0` on the job session (the same mapping the data editor relies on); nullability, defaults, identity, generated, enum values and lengths come from `table_detail`. | None |
| R14 | Upsert key | Upsert needs the primary key or a unique index whose columns are all NOT NULL and all mapped. MySQL `ON DUPLICATE KEY UPDATE col = VALUES(col)` (works on 5.7–8.4 and MariaDB; deprecation warning on 8.0.20+ is shown in the log), PG `ON CONFLICT (key) DO UPDATE SET col = EXCLUDED.col`; no non-key column mapped → PG `DO NOTHING`, MySQL `k = k`. MySQL with other unique indexes on the table: the preview warns that the row matching **any** unique key is updated. | Builder only |
| R15 | Transactions of a real import | PostgreSQL, and MySQL whose target has `atomicity = Yes` (transactional engine, no triggers; `dnt_query::atomicity`): the whole import is **one transaction**. Otherwise: autocommit per batch, preview says "Not transactional: batches already written stay if the import stops." | None |
| R16 | Skip mode and server errors | Conversion errors skip the row. A batch the server rejects: inside a transaction it is rolled back to a savepoint taken before it, then re-sent one row per statement (each under a savepoint) and the rows the server rejects are skipped and logged; outside a transaction skip mode sends **one row per statement** (a failing multi-row INSERT on a non-transactional table may have written part of its rows). Only data errors are row errors (PostgreSQL SQLSTATE classes 22 and 23; MySQL/MariaDB 1048, 1062, 1264, 1265, 1292, 1366, 1406, 1451, 1452, CHECK 3819 / MariaDB 4025); any other server error (a missing privilege or object, a syntax error) refuses every row alike and stops the job. Connection loss, timeout, deadlock, lock wait timeout and Stop always stop the job. A cap `max_errors` (default 1,000) stops the job when exceeded. | Speed of skip mode on MyISAM |
| R17 | Replace-all | Exactly framework §7.3: PG `DELETE FROM t` (or `TRUNCATE` when chosen) + inserts in one transaction; MySQL transactional target: `DELETE FROM t` + inserts in one transaction; MySQL `TRUNCATE` (implicit commit) or a non-transactional target: an **irreversible pre-step**, sent and committed before the inserts, confirmed separately ("Existing rows are deleted even if the import fails later"), Production types the table name for it. | None |
| R18 | Dry run | Always runs as a job, never sends `DELETE`/`TRUNCATE`/`INSERT` to the target. Step 1 parses the whole file and converts every cell. Step 2 (optional "Check on server") inserts every converted row into a session temporary table: MySQL `CREATE TEMPORARY TABLE <tmp> LIKE <t>`; PG `CREATE TEMP TABLE <tmp> (LIKE <t> INCLUDING DEFAULTS INCLUDING CONSTRAINTS INCLUDING IDENTITY INCLUDING GENERATED)` — IDENTITY/GENERATED added to §7.3's list so identity and generated columns behave as in the target — and on PG every column whose default calls `nextval(` gets `DROP DEFAULT, DROP NOT NULL` in the temp table so the target's sequences are not advanced. The temp table is dropped at the end (and dies with the session). Rows are inserted with append semantics whatever the mode (the temp table may lack the unique indexes upsert needs). The dry run collects errors (like skip mode) up to `max_errors` so the user sees all problems. Wizard copy states FK, unique-against-existing-rows and trigger checks are not covered. | None |
| R19 | Plans of generated jobs | An import or restore plan cannot hold every statement (files of GBs). The plan stores the builder configuration, the statement **templates** (pre-step SQL verbatim, `INSERT` head and tail verbatim, the first batch as a sample) and the SHA-256 of the file; the job regenerates statements with the same builder from the hash-verified file, so what runs is a deterministic function of what was previewed. The guard judges the templates at preview and again at start; a test asserts generated batches classify like their template. | None |
| R20 | Confirmation levels | Production: type the exact table name (import) or database name (restore), once per job, plus the irreversible pre-step acknowledgement. Staging: tick "I understand" / click confirm. Local and untagged: the review step's Start button. Read-only connections: import (real run and server-check dry run) and restore are blocked in the backend; file-only dry run and dumps are allowed. | UI only |
| R21 | File changed after preview | Start re-hashes the file (SHA-256, on a blocking thread) before anything is sent; a difference → `PlanStale`, nothing sent. The job hashes the bytes it reads again; inside a transaction a mismatch before `COMMIT` rolls back. Outside a transaction (or with an external tool) the window between the start check and the read is accepted and documented. | Theoretical TOCTOU |
| R22 | Integrated dump scope (v1) | Tables (structure and/or data) and views; PostgreSQL also the schemas, enum/domain types and standalone sequences the selected tables need. Skipped and listed in the dialog and the job log: routines, triggers, events, materialized views, PG partitioned/partition/inherited tables. For full fidelity use an external tool. | Dumps with those objects need external tools |
| R23 | Dump portability | MySQL: unqualified table names, `SET NAMES utf8mb4`, `SET SESSION sql_mode = 'NO_AUTO_VALUE_ON_ZERO'`, `SET time_zone = '+00:00'` (dump session too), `SET FOREIGN_KEY_CHECKS = 0` … `= 1`; view DDL loses its `DEFINER=` clause and the source-database qualifier on backquoted names. PG: schema-qualified names, `SET client_encoding = 'UTF8'`, `SET standard_conforming_strings = on`; tables are created without foreign keys, data loaded, sequences set (`setval`), then FKs added (`ALTER TABLE … ADD CONSTRAINT`), then views. | Dump format v2 |
| R24 | Dump snapshot | The dump reads in one read-only consistent snapshot: MySQL `START TRANSACTION WITH CONSISTENT SNAPSHOT, READ ONLY` (REPEATABLE READ), PG `BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY`. | None |
| R25 | External tool discovery | Whitelist `mysqldump`, `mariadb-dump`, `pg_dump`, `pg_restore`, `psql` only, looked up as `<name>.exe` (Windows) in the setting `external_tools_dir` first, then each `PATH` entry. Version from `<tool> --version` (5 s timeout) and shown in the dialog. No other program name is ever spawned. | None |
| R26 | External tool arguments | Built only from typed options by a pure function; no free-form argument from the UI. Object names starting with `-` or holding NUL/CR/LF are refused for external tools. MySQL tools get `--defaults-file=<secret file>` as the **first** argument (stricter than `--defaults-extra-file`: no global `my.ini` is read), `--protocol=TCP`, `--host`, `--port`, `--user`; PG tools get one `--dbname=<conninfo>` (no password) and `--no-password`. `psql` always `-X -v ON_ERROR_STOP=1`. | Arg builder only |
| R27 | External tool secrets | MySQL: option file `[client]` + `password="…"` (escaped). PG: pgpass file with one line `<host>:<port>:*:<user>:<password>` naming exactly the server and user the tool is started for (through an SSH tunnel the host is the tunnel's local end, `127.0.0.1`; every field has `\` and `:` escaped), so a `\connect` to another host inside a restored file never receives the password, and env `PGPASSFILE=<path>` (a path, not the secret). Files live in `<data_dir>/job-tmp/<job id>/`, created `create_new`, inside the user's profile (user-only ACL); deleted when the tool exits, on Stop and at next startup. The child environment drops every `PG*`, `MYSQL_*`, `LIBMYSQL_*` and `MARIADB_*` variable of the app's environment (incl. `PGPASSWORD`, `MYSQL_PWD`, `PGSERVICE`, `PGSERVICEFILE`); only the `PGPASSFILE` / `MYSQL_TEST_LOGIN_FILE` paths the app sets are passed. On Windows the clients read arguments and environment in the ANSI code page, so non-ASCII names (database, schema, table, user) and paths are refused for external tools with a clear message (use the built-in dump). The password never appears in arguments, env values, logs or the shown command line; stderr lines are masked for the password text. | None |
| R28 | External tools and TLS / SSH | Through an SSH tunnel the tool connects to the tunnel's local port; `verify-full` / `VERIFY_IDENTITY` cannot check the server name there and is refused with a clear message (use the integrated dump or `verify-ca`). | None |
| R29 | Output files | Every dump writes `<file>.dntpart` and renames it on success (as exports do); a failed or stopped job removes it. External tool output is read from the tool's stdout (so `.sql.gz` works with any tool) except `pg_dump --format=tar/custom` which also go through stdout. | None |
| R30 | Restore path choice | By content, never by extension: gzip is decompressed on the fly; `PGDMP` magic or a tar header with `toc.dat` → `pg_restore` (a gzip-wrapped archive is refused: pg_restore reads the file itself); text → N6 scan: `COPY … FROM stdin` or a `\` command line (PG) → `psql`; anything else → the app runner. No fourth path. Tool missing → refuse with the reason, nothing executed. Client commands: a backslash outside quoted text and comments anywhere in a statement is refused on both engines and both paths (psql would run `\!`, `\gexec`, `\set`, `\connect` … that follow text on a line; MySQL `\G`); on MySQL it is checked in the target server's own reading, so an executable comment gated above its version (mariadb-dump's `/*M!999999\- enable the sandbox mode */`) is not refused. psql lines allowed: `\connect` / `\c` to the target (or none / `-`) and `\restrict` / `\unrestrict` with exactly one alphanumeric key; the psql path also requires a pg_dump header (`-- PostgreSQL database dump` in the first 4 KiB). PostgreSQL string lexing: `SET [SESSION\|LOCAL] standard_conforming_strings` to anything but `on`, any mention of `standard_conforming_strings` / `backslash_quote` otherwise (`RESET`, `ALTER ROLE … SET`), `RESET ALL`, `DISCARD ALL` and `set_config(` whose first argument is not the literal `'search_path'` are refused (both paths). MySQL `sql_mode`: the scan and the job follow it with the same lexical tracker (every assignment of a `SET`, the last wins; literals, combination modes that include ANSI_QUOTES — ANSI, ORACLE, MSSQL, POSTGRESQL, DB2, MAXDB; user variables saved from `@@sql_mode`); anything it cannot predict is refused, and the job stops before sending when the session's dialect differs from the prediction. Residual risk: a function, trigger or event the file creates can change these settings when it runs later (e.g. during the restore's own inserts). | None |
| R31 | Restore target safety | Refused at preview from the scan, and again by the job for every statement before it is sent, so the typed confirmation covers what runs: `CREATE` / `DROP DATABASE` (MySQL also `SCHEMA`, MariaDB `CREATE OR REPLACE`), `ALTER DATABASE` and (MySQL) `USE` naming another database (PG names fold unquoted to lower case); MySQL `PREPARE`, `EXECUTE`, `DEALLOCATE` / `DROP PREPARE` and `CALL` (their statements are not visible to the scan); a MySQL statement that a `DELIMITER` merged from several (it is refused, and every piece of every reading is checked) unless it is a compound-body definition (`CREATE [OR REPLACE] [DEFINER=…] PROCEDURE/FUNCTION/TRIGGER/EVENT`); a file of the other engine. Not detected: statements that name another database by qualification (`DROP TABLE other.t`, `INSERT INTO other.t …`). PG app-runner restores run in one transaction unless the file has transaction control or statements that cannot run in a transaction block (then the preview says "A failure leaves a partial restore"); inside that transaction the job refuses such statements before sending (a file swapped after the start check). psql runs `--single-transaction` only when asked, when the file allows it and when it has no `\connect` (which leaves the first connection's transaction). MySQL restores are never transactional (DDL commits) and say so. Tool outcomes: psql exit 3 under `--single-transaction` + `ON_ERROR_STOP` → rolled back; every other non-zero exit (psql 1/2, any pg_restore failure), a kill or no exit code → "stopped part-way; the target may be partially restored" with `outcome_unknown`. | Refused files need editing |
| R32 | Restore Production / Staging | Production: type the target database name; Staging: confirm; read-only: blocked (all three paths). The plan binds file SHA-256 + size, connection runtime + tunnel generation + config, target database and options; any change → `PlanStale`. Plans are single-use, TTL as query plans. | None |
| R33 | N6 runner per statement | Each statement from the splitter is judged by the guard as a one-statement `ScriptPlan` (text verbatim, never re-split) and sent through `dnt_query::run_with`; a `Block` verdict stops the job before sending; `Confirm` verdicts are covered by the job confirmation. After each statement the session dialect is re-read and fed back to the splitter (a dump's `SET sql_mode` changes lexing). | None |
| R34 | `.sql.gz` with psql | Decompressed by the app and piped into `psql -f -`. | None |
| R35 | New dependencies | `csv` (CSV reader), `calamine` (XLSX reader, streaming cell reader), `flate2` (gzip; already in the lock through russh), `sha2` (already in the lock), tokio feature `process`. Justified in §9. | Swap a reader |
| R36 | Models of subagents | The plan tags tasks `[sonnet]`/`[opus]` as asked. In this environment only the default (Opus) model is reachable for subagents (`sonnet`/`haiku` overrides return model_not_found), so every task runs on the default model; the tags still record difficulty. | None |

## 3. Architecture

```
crates/core/src/sql/stream.rs      N6 StreamSplitter (pure)                      [P5a]
crates/core/src/sql/build.rs       + InsertOptions / upsert / OVERRIDING / row tuples / dump literals  [P5b]
crates/store/src/jobs.rs           jobs table access; MIGRATION_7 in db.rs; HistoryOrigin::{Import,Restore}
crates/transfer/  (dnt-transfer)   NEW pure crate: readers (csv/json/jsonl/xlsx), conversion, mapping,
                                   batch builder, dump rendering helpers, restore file detection + scan,
                                   external-tool discovery and argument building (N7, pure part)
src-tauri/src/jobs/                N5 JobManager, model, persistence glue, events
src-tauri/src/import_data/         import inspect / dry run / plan / run
src-tauri/src/backup/              integrated dump, restore (N6 runner), N7 process runner
src-tauri/src/commands/{jobs,import_data,backup}.rs   thin Tauri commands
ui/src/store/jobs.ts, ui/src/jobs/*                 Jobs panel, status bar counter
ui/src/transfer/import/*, ui/src/transfer/backup/*  Import wizard, Dump and Restore dialogs
e2e-web/specs/{jobs,import-wizard,backup}.spec.ts
```

Shared hotspots get small appended edits only: `src-tauri/src/lib.rs` (3 `pub mod` lines),
`src-tauri/src/commands/mod.rs` (module lines + appended command names + `.typ::<JobEvent>()`),
`src-tauri/src/session_manager.rs` (field `jobs` + one line in `close`), `src-tauri/src/state.rs`
(recovery call), `src-tauri/src/main.rs` (event sink in `setup`), `ui/src/bindings.ts` (regenerated),
`e2e-web/src/backend.ts` (appended handlers), the right-sidebar activity list, the tree context menu, the
status bar.

## 4. P5a — N5 job framework

### 4.1 Model (`src-tauri/src/jobs/model.rs`, exported to TS)

```rust
#[serde(rename_all = "snake_case")]
pub enum JobKind { Import, ImportDryRun, Dump, ToolDump, Restore, ToolRestore, Export }
#[serde(rename_all = "snake_case")]
pub enum JobState { Queued, Running, Done, Failed, Cancelled }
#[serde(rename_all = "snake_case")]
pub enum ProgressUnit { Rows, Bytes, Statements, None }
pub struct JobProgress { pub phase: String, pub done: f64, pub total: Option<f64>, pub unit: ProgressUnit }
#[serde(rename_all = "snake_case")]
pub enum JobLogLevel { Info, Warn, Error }
pub struct JobLogLine { pub at: String /* RFC 3339 UTC ms */, pub level: JobLogLevel, pub message: String,
                        #[serde(skip)] server: bool /* R6 */ }
pub struct JobSummary {
    pub id: Uuid, pub kind: JobKind, pub connection_id: Option<Uuid>, pub title: String,
    pub state: JobState, pub progress: JobProgress,
    pub created_at: String, pub started_at: Option<String>, pub finished_at: Option<String>,
    pub result_path: Option<String>, pub summary: Option<String>,
    pub error: Option<AppError>, pub outcome_unknown: bool, pub rerunnable: bool,
}
pub struct JobDetail { pub job: JobSummary, pub log: Vec<JobLogLine>, pub dropped_lines: u32,
                       pub params: Option<JobParams> }
pub struct JobEvent { pub job: JobSummary, pub new_log: Vec<JobLogLine> }   // payload of `job://progress`
/// The request a job was started with, for "Run again". The payload is the request type of §6/§7
/// (ImportRequest, DryRunRequest, DumpRequest, ToolDumpRequest, RestoreRequest) carried as a JSON value,
/// so the job framework does not depend on the import/backup modules.
#[serde(tag = "kind", content = "request", rename_all = "snake_case")]
pub enum JobParams { Import(serde_json::Value), ImportDryRun(serde_json::Value), Dump(serde_json::Value),
                     ToolDump(serde_json::Value), Restore(serde_json::Value), ToolRestore(serde_json::Value) }
```

Every `u64` crossing IPC is `f64` (`#[specta(type = f64)]`), as in the rest of the codebase.

### 4.2 JobManager (`src-tauri/src/jobs/manager.rs`)

`SessionManager` gains `pub(crate) jobs: JobManager` (constructed in `SessionManager::new` with the repo).
`JobManager` is `Clone` around `Arc<Inner>` so spawned tasks own it.

```rust
pub(crate) struct JobSpec { kind: JobKind, connection_id: Option<Uuid>, title: String,
                            params: Option<JobParams>, rerunnable: bool }
#[async_trait] pub(crate) trait JobRunner: Send + 'static {
    async fn run(self: Box<Self>, ctx: JobCtx) -> Result<JobDone, JobFailure>;
}
pub(crate) struct JobDone { pub summary: String, pub result_path: Option<String> }
pub(crate) struct JobFailure { pub error: DbError, pub outcome_unknown: bool, pub summary: String }
pub(crate) struct JobCtx { /* id, stop: StopSignal, manager handle */ }
impl JobCtx {
    fn id(&self) -> Uuid; fn stop(&self) -> StopSignal; fn is_stopped(&self) -> bool;
    fn progress(&self, phase: &str, done: u64, total: Option<u64>, unit: ProgressUnit);
    fn log(&self, level: JobLogLevel, message: impl Into<String>);
    fn log_server(&self, level: JobLogLevel, message: impl Into<String>, code: Option<&str>); // R6
    fn set_cancel(&self, handle: Option<CancelHandle>);       // the running statement's canceller
    fn set_kill(&self, kill: Option<Box<dyn Fn() + Send + Sync>>); // an external process
    fn tmp_dir(&self) -> Result<PathBuf, DbError>;             // <data_dir>/job-tmp/<instance>/<id>/, removed at end; refused once the job returned
}
impl JobManager {
    pub fn new(repo: Arc<ConnectionRepo>) -> Self;
    pub fn set_events(&self, sink: Arc<dyn JobEvents>);       // main.rs setup
    pub fn set_tmp_root(&self, dir: PathBuf);                 // state.rs
    pub fn recover(&self) -> Result<(), DbError>;             // R7, at startup
    pub(crate) fn submit(&self, spec: JobSpec, runner: Box<dyn JobRunner>) -> Result<Uuid, DbError>;
    pub(crate) fn track(&self, spec: JobSpec, cancel: Box<dyn Fn() + Send + Sync>) -> TrackedJob;
    pub async fn cancel(&self, id: Uuid) -> bool;
    pub async fn cancel_connection(&self, conn: Uuid);
    pub async fn stop_all(&self, wait: Duration);
    pub fn active_count(&self) -> u32;                        // queued + running (tracked included)
    pub fn list(&self, limit: u32) -> Result<Vec<JobSummary>, DbError>;  // live first, then persisted
    pub fn detail(&self, id: Uuid) -> Result<JobDetail, DbError>;
    pub fn clear_finished(&self) -> Result<(), DbError>;
}
pub trait JobEvents: Send + Sync { fn emit(&self, event: &JobEvent); }
pub(crate) struct TrackedJob { /* progress(rows), finish(Result<ExportSummary-like, DbError>) */ }
```

Behaviour:

- **submit**: inserts the job row (`queued`), emits an event, spawns a task that waits for a run slot of its
  connection (R2; a `Stop` while queued ends it `cancelled` without running), then sets `running`,
  `started_at`, calls `runner.run(ctx)`, and records `done` / `failed` / `cancelled`: `cancelled` when the
  stop signal was raised and the runner returned an error whose kind is `cancelled` or the stop was observed;
  `failed` otherwise. A runner panic becomes `failed` with an `internal` error (logged with location only, as
  `commands::guard` does).
- **cancel**: raises the job's `StopSignal`, then calls the registered `CancelHandle` and/or kill closure.
  Runners check `ctx.is_stopped()` between batches/statements and never send a statement once it is raised.
- **Sessions**: runners open their own session with `ConnectionRuntime::open_query_session(db)` (never a tab
  session or a data-pool session) and close it when done; they keep the runtime `Arc` and the tunnel
  generation to detect a replaced tunnel.
- **Events**: every state change, the final state and at most 5 progress/log events per second per job
  (coalesced; the latest progress wins; log lines accumulate in `new_log`).
- **Persistence**: insert at submit, update at start and finish (state, times, summary, error JSON, result
  path, outcome flag, persisted log per R6). SQLite calls run on the blocking pool; a persistence failure is
  logged (`tracing::warn!`) and never fails the job. The table keeps the newest 500 finished jobs.
- **Tracked jobs** (R3): `track` registers a `running` job immediately (no slot), `progress` updates
  rows, `finish` records the outcome; Stop calls the given cancel closure.

### 4.3 Commands (`src-tauri/src/commands/jobs.rs`)

| Command | Returns | Notes |
|---|---|---|
| `job_list(limit: Option<u32>)` | `Vec<JobSummary>` | live jobs (queued/running) first, then history; default 200, max 500 |
| `job_detail(id)` | `JobDetail` | log + params |
| `job_cancel(id)` | `bool` | false when not live |
| `job_active_count()` | `u32` | close guard, status bar |
| `job_stop_all()` | `()` | close guard; waits ≤ 5 s |
| `job_reveal_result(id)` | `()` | reveals `result_path` in the file manager, only when the job recorded it and the file exists |
| `job_clear_history()` | `()` | finished jobs only |
| `get_jobs_per_connection` / `set_jobs_per_connection(n)` | `u32` / `()` | 1–8, default 2 |
| `get_external_tools_dir` / `tools_pick_dir()` / `clear_external_tools_dir()` | `Option<String>` / `Option<String>` / `()` | picked only in the backend's folder dialog (tools run from it; the UI never names it); none = `PATH` only |

### 4.4 Store (`crates/store`)

`MIGRATION_7`:

```sql
CREATE TABLE jobs (
  id TEXT PRIMARY KEY,
  kind TEXT NOT NULL CHECK (kind IN ('import','import_dry_run','dump','tool_dump','restore','tool_restore','export')),
  connection_id TEXT NULL REFERENCES connections(id) ON DELETE SET NULL,
  title TEXT NOT NULL, params_json TEXT NULL, rerunnable INTEGER NOT NULL DEFAULT 0,
  state TEXT NOT NULL CHECK (state IN ('queued','running','done','failed','cancelled')),
  created_at TEXT NOT NULL, started_at TEXT NULL, finished_at TEXT NULL,
  result_path TEXT NULL, summary TEXT NULL, error_json TEXT NULL,
  outcome_unknown INTEGER NOT NULL DEFAULT 0, log_json TEXT NOT NULL DEFAULT '[]', dropped_lines INTEGER NOT NULL DEFAULT 0);
CREATE INDEX jobs_created ON jobs(created_at DESC);
-- then the query_history rebuild of MIGRATION_6 with origin IN (…,'table_design','import','restore')
```

`crates/store/src/jobs.rs`: `JobRecord` (plain struct mirroring the row), `ConnectionRepo::{job_insert,
job_update, job_list(limit), job_get(id), job_mark_interrupted() -> u32, job_clear_finished(), job_prune(keep)}`.
`HistoryOrigin::{Import, Restore}` ↔ `"import"`, `"restore"`; History panel labels "Import", "Restore".

### 4.5 UI

- `ui/src/store/jobs.ts` (Zustand `useJobs`): `jobs: Record<string, JobSummary>`, `order`, `load()`,
  `subscribe()` (listens to `job://progress` through `@tauri-apps/api/event`, ignored outside Tauri), the
  2 s poll while anything is live (R4), `cancel(id)`, `detail(id)`, `activeCount`.
- Right sidebar: new enabled activity **Jobs** (icon list-checks) next to History; the panel shows live jobs
  first (title, connection, phase, progress bar with `done/total` and unit, elapsed, Stop) and finished jobs
  (state chip Done / Failed / Cancelled / "Outcome unknown", summary, finished time); a row opens a detail
  view with the log (monospace, newest at the bottom, "n lines not shown"), **Open folder** (result file),
  **Run again** (R5, when `rerunnable`), **Copy log**; header actions Clear finished.
- Status bar: a counter "n jobs" (spinner) while `activeCount > 0`; click opens the Jobs panel.
- App close guard (`installCloseGuard`): before the staged-edit/design checks, `job_active_count() > 0` asks
  `{kind:'jobsRunning', count}` → "Stop jobs and quit" calls `job_stop_all` then continues.
- Settings → Query & data: "Background jobs per connection" (1–8); "External tools folder" (text + Browse
  using the existing folder dialog; empty = PATH only).

## 5. P5a — N6 streaming splitter (`crates/core/src/sql/stream.rs`)

```rust
pub struct SplitOptions { pub detect_client: bool, pub max_statement_bytes: usize /* 256 MiB */ }
pub struct StreamStatement { pub text: String, pub start: u64, pub end: u64 }
pub enum SplitEvent {
    Statement(StreamStatement),
    /// `COPY … FROM stdin` data block (PG, detect_client): bytes after the COPY statement's delimiter
    /// up to and including the `\.` line (or end of input).
    CopyData { start: u64, end: u64 },
    /// A line starting with `\` (only spaces/tabs before it) outside quoted text/comments (detect_client).
    ClientCommand(StreamStatement),
}
pub enum SplitError { InvalidUtf8 { offset: u64 }, StatementTooLarge { start: u64, limit: usize } }
pub struct StreamSplitter { … }
impl StreamSplitter {
    pub fn new(dialect: Dialect, opts: SplitOptions) -> Self;
    pub fn set_dialect(&mut self, d: Dialect);  // applies to everything not yet returned
    pub fn push(&mut self, chunk: &[u8]);
    pub fn finish(&mut self);                     // no more input
    pub fn next_event(&mut self) -> Result<Option<SplitEvent>, SplitError>; // None: needs input / done
    pub fn is_done(&self) -> bool;
}
pub fn is_copy_from_stdin(stmt: &str) -> bool;    // PG: COPY … FROM STDIN at depth 0
```

Rules:

1. Same lexer as `split` (`lexer::skip_at`, `delimiter_directive`, `leading_blank_len`), same trimming, blank
   fragments dropped, offsets are absolute byte offsets in the stream; `text == input[start..end]`.
2. Lazy: `next_event` scans only as far as needed for one event, so `set_dialect` between two calls applies
   exactly to the rest (a MySQL `SET sql_mode` changes `ANSI_QUOTES`/`NO_BACKSLASH_ESCAPES`).
3. A decision at byte `i` is taken only when every byte it can look at is present: positions are scanned
   only before the last `\n` of the buffer (all lookaheads of `split` — doubled quotes, `--` + blank,
   `E'`, dollar tags, the `DELIMITER` line, a delimiter token — end within their line), and a construct whose
   end is the end of the buffer waits for more input until `finish`. Unterminated constructs are rescanned
   only after the buffer has doubled since the last attempt (amortised linear time). The one byte before
   the segment and whether its line was blank so far are kept, because `split` looks at them
   (`prev_is_ident`, `DELIMITER` at line start).
4. Memory: the buffer holds the current statement plus the unscanned tail; a statement over
   `max_statement_bytes` is `StatementTooLarge`.
5. Detection (`detect_client`): a `Statement` that `is_copy_from_stdin` (PG dialect only) is followed by a
   `CopyData` event, and splitting resumes after the `\.` line; a `\` at a line start outside constructs
   first flushes the pending non-blank segment as a `Statement`, then returns the line as `ClientCommand`.
6. Tests: every case of `split.rs` tests and every `testdata/sql/*.sql` file split with chunk sizes 1, 2, 3,
   7, 64, 4096 equals `split`; a proptest generating scripts from SQL fragments (`'`, `"`, `` ` ``, `$$`,
   `$t$`, `E'`, `--`, `-- `, `#`, `/*`, `*/`, `\`, CR, LF, `DELIMITER //` lines, `//`, multibyte chars,
   words) with random chunk boundaries (also inside UTF-8 sequences) under MySQL (all four mode
   combinations) and PG equals `split`; detection tests (pg_dump-like COPY block, `\connect`, `\restrict`
   line, MySQL `\G` refusal source); a 64 MiB single-line input is linear (< 2 s in debug).

## 6. P5b — Import

### 6.1 Readers (`dnt-transfer::read`)

```rust
pub enum SourceFormat { Csv, Json, Jsonl, Xlsx }
pub struct SourceOptions { pub format: SourceFormat, pub delimiter: Option<char> /* csv; None = guess from first line among , ; \t | */,
                           pub header: bool /* csv, xlsx */, pub sheet: Option<String> /* xlsx */ }
pub enum Cell { Null, Text(String), Number(String) /* JSON / XLSX number text */, Bool(bool),
                Json(String) /* nested object/array */, DateTime(String) /* XLSX date, ISO */ }
pub struct Record { pub number: u64 /* 1-based data row (CSV line of record start) */, pub cells: Vec<Cell> }
pub trait RecordReader: Send { fn columns(&self) -> &[String];
                               fn next_record(&mut self) -> Result<Option<Record>, ReadError>;
                               fn bytes_read(&self) -> u64; fn total_bytes(&self) -> u64; }
pub fn open(path: &Path, opts: &SourceOptions) -> Result<Box<dyn RecordReader>, ReadError>;
pub struct Inspection { pub columns: Vec<String>, pub rows: Vec<Record> /* ≤ 100 */, pub sheets: Vec<String>,
                        pub delimiter: Option<char>, pub total_bytes: u64 }
pub fn inspect(path: &Path, opts: &SourceOptions) -> Result<Inspection, ReadError>;
```

- CSV: `csv` crate, strict field count (a record with another count → `ReadError::Record { number, message }`),
  no header → columns `column_1…n`; duplicate/empty header names get `_2` / `column_n`.
- JSON: streaming over the top-level array (serde `SeqAccess`), each element must be an object; JSONL: one
  object per non-blank line. Columns = keys in first-seen order over the first 1,000 objects; a later new
  key is ignored with a warning in the job log; a missing key = `Cell::Null`.
- XLSX: `calamine` streaming cell reader on one sheet; empty → Null, string → Text, int/float → Number
  (integers without `.0`), bool → Bool, date/datetime → DateTime (ISO, seconds), error cell → `ReadError`.
- `ReadError` carries the record number and never the cell value.

### 6.2 Conversion and mapping (`dnt-transfer::convert`, `::mapping`)

```rust
pub struct TargetColumn { pub name: String, pub db_type: String, pub logical: LogicalType, pub nullable: bool,
    pub has_default: bool, pub auto_increment: bool, pub identity: Option<IdentityKind>, pub generated: bool,
    pub enum_values: Vec<String>, pub max_chars: Option<u32> /* char/varchar(n) */, pub collation: Option<String> }
pub enum EmptyAs { Null, EmptyString }
pub enum BinaryEncoding { Hex /* 0x… or bare hex */, Base64, Utf8Text }
pub struct ConvertOptions { pub empty: EmptyAs, pub null_text: Option<String>, pub trim: bool, pub binary: BinaryEncoding }
pub fn convert(cell: &Cell, col: &TargetColumn, opts: &ConvertOptions, kind: DbKind) -> Result<FullValue, CellError>;
pub struct Mapping { pub target: String, pub source: Option<u32> /* source column index */ }
pub fn auto_map(source: &[String], targets: &[TargetColumn]) -> Vec<Mapping>; // exact, then ASCII case-insensitive, then normalised (alnum, lower)
pub fn check_mapping(m: &[Mapping], targets: &[TargetColumn], mode: &ImportMode) -> MappingReport { errors, warnings }
```

Per logical type: Integer (i64/u64 range, unsigned columns refuse negatives), Decimal (decimal syntax), Float,
Bool (`true/false/1/0/yes/no/t/f/y/n`, case-insensitive), Text (`max_chars` checked in chars), Date
(`YYYY-MM-DD`), Time, DateTime (`YYYY-MM-DD[ T]HH:MM[:SS[.f{1,6}]]`, `T` → space; timestamptz keeps an
offset), Json (must parse), Uuid (8-4-4-4-12 hex), Binary (per `BinaryEncoding`), Enum (one of
`enum_values`, case-sensitive), Array/Other → `CellError::Unsupported` (PG: text passed with a cast is a
follow-up). `null_text` match → NULL; empty cell → `EmptyAs` (text columns) / NULL (others). NULL into a
NOT NULL column without default → error. Mapping errors: unknown/duplicate target, generated column
mapped, PG identity `ALWAYS` mapped without "Override identity", NOT NULL column without default not
mapped, upsert key not mapped or not a valid key (R14). `CellError { column, reason }` never holds the value.

### 6.3 Batches (`dnt-transfer::batch`)

`BatchBuilder::new(table: TableRef, cols: Vec<ColumnTarget>, opts: InsertOptions, dialect, max_rows /* 1,000 */,
max_bytes /* 1 MiB */)`, `push(row: Vec<FullValue>) -> Result<Option<Batch>, BuildError>`,
`flush() -> Option<Batch>`, `Batch { sql: String, rows: u32, first_record: u64, records: Vec<u64> }`.
A batch equals `build::insert_sql_with(&table, &cols, &rows, &opts, dialect)` for the same rows (test).
`template()` returns `(head, tail)` for the preview (R19).

### 6.4 Core builder additions (`crates/core/src/sql/build.rs`)

```rust
pub enum OnConflict { Error, Update { key: Vec<String>, update: Vec<String> } }
pub struct InsertOptions { pub overriding_system_value: bool, pub on_conflict: OnConflict }
pub fn insert_sql_with(t, cols, rows, opts: &InsertOptions, d) -> Result<String, BuildError>;
pub fn insert_head(t, cols, opts, d) -> Result<String, BuildError>;   // "INSERT INTO … (…) [OVERRIDING SYSTEM VALUE] VALUES "
pub fn insert_tail(cols, opts, d) -> Result<String, BuildError>;      // "" or " ON DUPLICATE KEY UPDATE …" / " ON CONFLICT (…) DO …"
pub fn row_tuple(cols, row, d) -> Result<String, BuildError>;         // "(lit, lit, …)"
pub fn dump_literal(v: &FullValue, col: &ColumnTarget, d: Dialect) -> Result<String, BuildError>;
```

`dump_literal` = `literal` plus PG `Value::Array` → `'<array text>'::<db_type>` and PG `Value::Other` →
`'<text>'::<db_type>` (cast target rendered by the existing `pg_cast_target`); MySQL `Other` stays
`Unsupported` (the dump reports the column and suggests `mysqldump`). The existing `insert_sql` is unchanged.

### 6.5 Flow and commands (`src-tauri/src/import_data/`, `commands/import_data.rs`)

| Command | Does |
|---|---|
| `import_pick_file()` | Backend open dialog (CSV/TSV/TXT, JSON/JSONL/NDJSON, XLSX, All); admits the path for `FileAccess::ImportRead`; returns `Option<String>` |
| `import_inspect(path, source)` | `Inspection` (≤ 100 rows, cells shown as text) |
| `import_target(connection_id, table: ObjectRef)` | `ImportTarget { columns: Vec<TargetColumn>, keys: Vec<KeyChoice>, atomicity, env, read_only, server }` read on a short-lived dedicated session (R13) |
| `import_auto_map(source: Vec<String>, targets: Vec<TargetColumn>)` / `import_check_mapping(mapping, targets, mode)` | pure helpers over `dnt-transfer::mapping` (no DB), so the UI never re-implements mapping rules |
| `import_dry_run(request: DryRunRequest)` | submits an `ImportDryRun` job; `DryRunRequest { request: ImportRequest, server_check: bool }`; server check needs a non-read-only connection and a confirmed guard verdict (`confirmed: bool`) |
| `import_prepare(request: ImportRequest)` | `ImportPreview` plan (below) |
| `import_start(plan_id, confirm: ImportConfirm)` | re-checks (R21, R32-like context) and submits an `Import` job; returns job id |

```rust
pub struct ImportRequest { pub connection_id: Uuid, pub table: ObjectRef, pub path: String, pub source: SourceOptions,
    pub mapping: Vec<Mapping>, pub mode: ImportMode, pub errors: ErrorPolicy, pub convert: ConvertOptions,
    pub batch_rows: u32, pub batch_bytes: u32, pub override_identity: bool }
#[serde(tag = "kind")] pub enum ImportMode { Append, Replace { truncate: bool }, Upsert { key: Vec<String> } }
#[serde(tag = "kind")] pub enum ErrorPolicy { Stop, Skip { max_errors: u32 } }
pub struct ImportPreview { pub plan_id: Uuid, pub file: FileFacts /* name, size, sha256 */, pub table: ObjectRef,
    pub transactional: bool, pub pre_step: Option<PreStep> /* sql, irreversible, warning */,
    pub insert_head: String, pub insert_tail: String, pub sample: Option<String>,
    pub warnings: Vec<String>, pub confirm: ConfirmLevel /* none | click | type_name{name} */,
    pub irreversible_name: Option<String>, pub expires_in_seconds: u32 }
pub struct ImportConfirm { pub confirmed: bool, pub name: Option<String>, pub irreversible_ack: bool }
```

Plan (`ImportPlanStore`, single use, TTL = `plan_ttl`, max 64): request, file facts, table fingerprint
(`dnt_query::fingerprint` of a fresh `table_detail`), connection runtime `Arc` + tunnel generation + config,
dialect, templates, guard verdicts. Start re-checks: runtime/config/env/read-only, fresh fingerprint, file
SHA-256 (R21), confirmation (R20), guard on templates (`Block` → `Blocked`).

### 6.6 Execution (Import job)

1. Open session on the table's database; MySQL reads `sql_mode` (dialect); PG
   `SET standard_conforming_strings = on` (session) as the table designer does.
2. Fresh fingerprint check (stale → `PlanStale`, nothing sent).
3. Transaction per R15 (`BEGIN` / `START TRANSACTION`); pre-step per R17 (irreversible ones sent and
   committed first, in autocommit, and logged).
4. Read records → map → convert (errors per policy, logged `Row n, column c: reason`) → `BatchBuilder`;
   each batch: stop check, (savepoint in skip mode inside a transaction), send, progress (rows, bytes of
   file read).
5. End: file hash compare (R21) → `COMMIT` → summary "Imported N rows (M skipped) into t in K batches".
6. Failure outcomes (N2d): in a transaction before `COMMIT` was sent → `ROLLBACK`; reported "rolled back"
   only when the server confirmed it, else "Not committed; the server rolls the transaction back when it
   notices the lost connection". `COMMIT` sent without answer → `outcome_unknown`. Outside a transaction:
   "K batches were written before the error"; the in-flight batch on lost connection/timeout →
   `outcome_unknown`. No retry.
7. One History entry (R10): `-- Import <file> into <table>: <mode>, N rows, K batches` + pre-step + head +
   `…` + tail, kind `write`, status, error kind/code, `outcome_unknown`.

The dry-run job follows the same reading/conversion; with server check it runs `BEGIN`, creates the temp
table (R18), inserts with savepoints and row isolation, then `ROLLBACK` and `DROP` (PG temp dropped by the
rollback; MySQL `DROP TEMPORARY TABLE`). Summary: "N rows valid, M rows rejected (first errors in the log)".

### 6.7 Wizard UI (`ui/src/transfer/import/`)

A dialog-sized wizard (`Modal`, steps on the left: Source · Target · Mapping · Options · Check · Review):

1. **Source**: file picker, format (auto from extension, changeable), delimiter/header/sheet, preview grid of
   the first 100 rows.
2. **Target**: connection, database/schema, table (pre-filled from the tree: table context menu "Import
   data…"); target column list with type/null/default/identity flags.
3. **Mapping**: one row per target column: source column select (auto-mapped; "Auto map" / "Clear"),
   required markers, errors/warnings inline; unmapped columns use their default.
4. **Options**: mode (Append / Replace all [Delete rows | Truncate] / Upsert [key select]), on error
   (Stop / Skip and log, max errors), empty cells, NULL text, binary encoding, batch rows and size, Override
   identity (PG, when relevant).
5. **Check** (optional, recommended): "Validate file" and "Validate on server (temporary table)" → dry-run job,
   live progress and the result summary + first errors; notes on what is not checked.
6. **Review**: `import_prepare` result: file name/size/SHA-256 prefix, target, transactional or not, pre-step
   SQL with the irreversible warning and acknowledgement, INSERT template and sample (read-only code
   blocks), warnings, confirmation per R20 → **Start import** → job id, the wizard closes and the Jobs panel
   opens on the job.

## 7. P5c — Dump, external tools, restore

### 7.1 Integrated dump (`backup/dump.rs`, rendering helpers in `dnt-transfer::dump`)

```rust
pub struct DumpRequest { pub connection_id: Uuid, pub database: String, pub schemas: Vec<String> /* PG */,
    pub objects: Vec<DumpObject>, pub include_drop: bool, pub compress: bool, pub path: String }
pub struct DumpObject { pub schema: Option<String>, pub name: String, pub kind: ObjectKind /* Table|View|Sequence|Type */,
                        pub data: bool /* tables */ }
```

Commands: `dump_objects(connection_id, database, schemas)` → selectable objects + skipped ones with reasons
(R22); `dump_pick_path(base_name, compress)` (save dialog, admits `FileAccess::DumpWrite`); `dump_start(request)`
→ job id (no confirmation: a read; allowed on read-only connections).

Job: session on the database, snapshot (R24), MySQL `SET time_zone = '+00:00'`; header

```
-- DntDatabase dump v1
-- Source: <MySQL|MariaDB|PostgreSQL> <version>, database <name>[, schemas a, b]
-- Created: <RFC 3339 UTC>
-- Objects: <n> tables (<m> with data), <v> views[, <s> sequences, <t> types]
```

then the session statements of R23, objects in order — PG: schemas, types, sequences, tables (no FKs, plus
their indexes and comments), data, `setval`s, FKs, views; MySQL: tables (FK order, `SHOW CREATE TABLE` text),
data, views (textual dependency order) — and `-- End of dump: <k> statements`. Tables are ordered
topologically by FK among selected tables (ties and cycles by name). Data is read with `CellMode::Full`
(`max_cell_bytes` 256 MiB), written as multi-row `INSERT` via `dump_literal` in batches ≤ 1 MiB / 1,000 rows,
generated columns omitted, PG identity `ALWAYS` tables with `OVERRIDING SYSTEM VALUE`. Every statement ends
with `;\n` (MySQL view DDL has no `DELIMITER` need). Output per R29; `.sql.gz` through `flate2`. Progress:
rows per table; result path = the file.

### 7.2 N7 external tools (`dnt-transfer::tools` pure, `backup/ext_tools.rs` process)

```rust
pub enum Tool { MysqlDump, MariadbDump, PgDump, PgRestore, Psql }
pub struct ToolInfo { pub tool: Tool, pub path: String, pub version: String, pub mariadb: bool }
pub fn find_tool(tool: Tool, configured: Option<&Path>, path_env: Option<&OsStr>) -> Option<PathBuf>;
pub struct ToolConn { pub kind: DbKind, pub host: String, pub port: u16, pub user: String, pub database: Option<String>,
                      pub ssl: Option<SslConfig>, pub tunnelled: bool }
pub struct MysqlDumpOptions { pub tables: Vec<String>, pub structure: bool, pub data: bool, pub routines: bool,
    pub events: bool, pub triggers: bool, pub single_transaction: bool }
pub struct PgDumpOptions { pub schemas: Vec<String>, pub tables: Vec<(String, String)>, pub structure: bool, pub data: bool,
    pub format: PgDumpFormat /* plain|custom|tar */, pub no_owner: bool, pub no_privileges: bool }
pub struct PsqlOptions { pub single_transaction: bool }
pub struct PgRestoreOptions { pub single_transaction: bool, pub clean: bool, pub no_owner: bool, pub no_privileges: bool }
pub struct Invocation { pub program: PathBuf, pub args: Vec<OsString>, pub env_remove: Vec<&'static str>,
                        pub env_set: Vec<(&'static str, OsString)>, pub secret: SecretFileSpec /* path + contents kind */,
                        pub stdin_file: bool, pub stdout: StdoutUse /* file | list | discard */ }
pub fn mysql_dump_args(info: &ToolInfo, conn: &ToolConn, server: &ServerInfo, o: &MysqlDumpOptions, secret: &Path) -> Result<Invocation, ToolError>;
pub fn pg_dump_args(…); pub fn psql_args(…); pub fn pg_restore_args(…); pub fn pg_restore_list_args(file);
pub fn mysql_option_file(password: &str) -> String; pub fn pgpass_line(password: &str) -> String;
pub fn shown_command(inv: &Invocation) -> String;   // what the preview/log shows; never a secret
```

Argument rules (R26) plus: mysqldump `--default-character-set=utf8mb4 --hex-blob`, `--single-transaction`,
`--routines`, `--events`, `--triggers`/`--skip-triggers`, `--no-data` / `--no-create-info`, MySQL 8+ client
against MariaDB or MySQL 5.7 adds `--column-statistics=0`, a MySQL client adds `--set-gtid-purged=OFF`;
the database then the tables, after the options. TLS from the connection's `SslConfig`
(MySQL client `--ssl-mode=…`, MariaDB client `--ssl`/`--skip-ssl`/`--ssl-verify-server-cert`, CA/cert/key
paths; PG `sslmode`, `sslrootcert`, `sslcert`, `sslkey` in conninfo; R28). pg_dump `--format=p|c|t`,
`--encoding=UTF8`, `--schema=<"pattern">`, `--table=<"schema"."name">` (patterns double-quoted, `"`
doubled), `--schema-only`/`--data-only`, `--no-owner`, `--no-privileges`. psql `-X --no-password -v
ON_ERROR_STOP=1 [--single-transaction] --dbname=… -f -`. pg_restore `--no-password --exit-on-error
[--single-transaction] [--clean --if-exists] [--no-owner] [--no-privileges] --dbname=… <file>`; `--list <file>`
for the preview.

Runner: `tokio::process::Command` (`kill_on_drop`, Windows `CREATE_NO_WINDOW`), stdin fed from the file
(decompressed when gzip) on a blocking thread, stdout to the `.dntpart` file (gzip for `.sql.gz`) or captured
(`--list`, ≤ 4 MiB), stderr lines into the job log (password masked, ≤ 2,000 lines kept by the job log).
Stop = kill. Outcomes (framework §7.6.8): exit 0 → done; non-zero with `--single-transaction`/ON_ERROR_STOP
→ "failed; the tool reported an error, the transaction was rolled back"; non-zero otherwise, killed or lost →
"stopped part-way; the target may be partially restored" with `outcome_unknown`.

Commands: `tools_list()` → found tools + versions (and the searched folders); `tool_dump_start(request:
ToolDumpRequest { connection_id, database, tool, mysql: Option<MysqlDumpOptions>, pg: Option<PgDumpOptions>,
compress, path })` → job (a read; allowed on read-only).

### 7.3 Restore (`backup/restore.rs`, detection/scan in `dnt-transfer::detect`)

```rust
pub enum RestorePath { AppRunner, Psql, PgRestore }
pub struct ScanStats { pub statements: u64, pub reads: u64, pub writes: u64, pub ddl: u64, pub transaction: u64,
    pub other: u64, pub drops: u64, pub copy_blocks: u64, pub client_commands: u64, pub app_dump: bool }
pub struct RestoreRequest { pub connection_id: Uuid, pub database: String, pub path: String, pub single_transaction: bool,
    pub clean: bool /* pg_restore */, pub no_owner: bool /* pg_restore */ }
pub struct RestorePreview { pub plan_id: Uuid, pub path: RestorePath, pub file: FileFacts, pub compressed: bool,
    pub stats: Option<ScanStats>, pub toc: Vec<String> /* pg_restore --list, ≤ 2,000 lines */, pub toc_truncated: bool,
    pub command: Option<String> /* shown_command */, pub tool: Option<ToolInfo>, pub transactional: bool,
    pub warnings: Vec<String>, pub confirm: ConfirmLevel, pub expires_in_seconds: u32 }
```

Commands: `restore_pick_file()` (admits `FileAccess::RestoreRead`), `restore_preview(request)`,
`restore_start(plan_id, confirmed: bool, name: Option<String>)` → job id.

Preview: blocked on read-only (`Blocked`); detection (R30) and scan in one blocking pass that also computes
SHA-256; app-runner refusals (R31); psql/pg_restore needed but not found → `Unsupported` with "This file
needs psql (it uses COPY … FROM stdin or psql commands) and psql was not found. Install the PostgreSQL client
tools or set the External tools folder." — nothing executed; MySQL target + PG file (or the reverse, from
the header/scan) → refused; plan stored (R32).

App-runner job: session on the target database (MySQL `open_query_session(Some(db))`), PG transaction per
R31, BOM stripped, gzip decoded, `StreamSplitter` (detect on) fed in 256 KiB chunks, R33 per statement,
progress = bytes read / file size and statement count, hash compared before `COMMIT` (PG) / at the end. Stop
between statements raises the stop signal (the running statement is cancelled through the session's
handle). Outcome mapping as §6.6. History (R10) `-- Restore <file> (<sha256 prefix>) into <db>: N statements`.

Tool job: N7 runner with the plan's invocation; History entry likewise.

### 7.4 UI (`ui/src/transfer/backup/`)

- Tree context menu: database (MySQL) / database and schema (PG): **Dump…**, **Restore…**; table: **Import
  data…**, **Dump table…** (dump dialog pre-selecting the table).
- **Dump dialog**: method segmented control (Built-in · mysqldump/mariadb-dump/pg_dump, with found version
  or "not found" + the folders searched); object checklist grouped by kind with "data" toggles for tables
  and the skipped list (reasons); options (drop statements, compress; tool options per §7.2); output file
  (Browse → save dialog) → **Start dump** → job.
- **Restore dialog**: file (Browse), target connection + database, **Analyze** → preview: path chip
  ("Built-in runner" / "psql" / "pg_restore"), statement statistics or the TOC (virtualised list), command
  line (masked), transactional or the partial-restore warning, warnings, confirmation (R32: Production types
  the database name, Staging ticks a checkbox) → **Start restore** → job. `PlanStale` → "The file or the
  connection changed; analyze again."

## 8. Testing and completion criteria

| Layer | What |
|---|---|
| Pure Rust | N6 equivalence + proptest + detection; builder additions; readers (CSV quoting, BOM, field counts, JSON streaming, JSONL, XLSX fixture built in the test with `rust_xlsxwriter`); conversion per type; mapping; batch == `insert_sql_with`; dump rendering (header, order, view rewrite); tool discovery and argument building (every option, TLS, tunnel refusal, unsafe names, secret file formats, masking); restore detection (gzip, PGDMP, tar, COPY, `\connect`, app header) |
| Fake session | JobManager (limit, queue, Stop queued/running, events throttling, persistence, recover, panic → failed); import executor (stop/skip, savepoint isolation, rollback, commit lost → unknown, pre-step order, PlanStale before any statement); restore runner (Block stops before sending, dialect feedback, hash mismatch → rollback) |
| DB (`DNT_TEST_DB=1`, MySQL 5.7/8.0, MariaDB 10.6/11.4, PG 12/17) | import append/upsert/replace on every server; dry run leaves the table intact (row count + checksum before/after, also replace mode on MySQL, and PG sequence value unchanged); dump → restore into a `p5_` database gives identical rows per table (exact `FullValue` comparison ordered by key) on every server; BLOB 10 MiB export → import byte-exact; mysqldump (installed locally) dump → app-runner restore on MySQL 8; pg_dump-style plain file with `COPY` → refused when psql is missing, zero statements sent; read-only connection blocks restore; file changed after preview → `PlanStale`. All objects use the `p5_` prefix and are dropped. |
| UI (Vitest) | jobs store (events + poll), Jobs panel, status counter, close guard; wizard steps/state/validation; dump and restore dialogs |
| Browser E2E (`E2E_WEB_PORT=5181`) | `jobs.spec.ts` (live job progress, Stop, detail, Run again), `import-wizard.spec.ts` (file → map → dry run → review → start), `backup.spec.ts` (dump and restore preview + confirmation) |

Completion (framework §7.7, all required): export of 1M rows below +200 MB (measured once, recorded in the
final review), BLOB 10 MiB round-trip, CSV import with mapping and upsert, dry run keeps the target intact
(count + checksum, also replace on MySQL), integrated dump/restore identical on 6 servers, pg_dump plain →
N6 detects and routes to psql or refuses with nothing run, N7 restore blocked on read-only, file changed →
`PlanStale`. Gates: fmt, clippy `-D warnings`, `cargo test --workspace`, the P5 DB tests with
`DNT_TEST_DB=1`, `pnpm --dir ui lint`, `pnpm --dir ui test`, `pnpm --dir e2e-web typecheck`, the e2e-web
suite, bindings current.

## 9. Dependencies

| Crate | Why | Alternatives rejected |
|---|---|---|
| `csv` (BurntSushi) | RFC 4180 reader with exact quoting, streaming, tiny, no unsafe in our use | hand-written parser (risk, more tests) |
| `calamine` (`default-features = false`) | Only maintained pure-Rust XLSX reader; streaming cell reader keeps memory bounded; dates/shared strings handled | hand-written zip + XML reader (dates, styles, inline strings: risky) |
| `flate2` | gzip read/write for `.sql.gz`; already in `Cargo.lock` via russh | none |
| `sha2` | file SHA-256 for plans; already in `Cargo.lock` | — |
| tokio `process` feature | spawn/kill external tools asynchronously | std process + threads |

No other runtime dependency. `proptest` (already a dev-dependency of `dnt-core`) drives the N6 fuzz test.

## 10. Out of scope and follow-ups

- Import: other encodings, `.xls`/ODS, arrays of arrays, PG array/composite targets, `COPY FROM STDIN` fast path,
  per-column transforms, a separate rejected-rows file.
- Integrated dump of routines, triggers, events, materialized views, partitions; dump format v2 portability.
- `pg_dump --format=directory`; `mysql` client as a restore path; restore of a dump into a database of
  another name with explicit cross-database references rewritten.
- Export jobs on dedicated sessions (R3) and a multi-table export.
- Preview scans of very large files run synchronously in the command (with a spinner); a scan job with
  progress is a follow-up.
