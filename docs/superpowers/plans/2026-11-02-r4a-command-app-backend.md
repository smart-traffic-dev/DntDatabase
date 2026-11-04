# R4a — Command family in the app backend Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A connection whose driver is a Command driver can be used end-to-end through the app's backend commands:
- browse its tree;
- plan, guard and run console scripts, with events, History and redaction;
- read and edit data pages;
- browse and edit keys;
- save Favorites with the language's secret check.

Everything is proven with `FakeCommandDriver` registered as `DbKind::Redis`. UI work is R4b.

**Architecture:** Every SQL-only path in `src-tauri` gains an explicit Command arm (`match session.engine()`), built on the R2 contracts (`plan_commands` / `guard_commands` / `run_commands`, `DataAccess`, `Keyspace`, `check_read`).
- **Language without a session:** a driver exposes its `CommandLanguage` through `Driver::command_language()`, so splitting, redaction and Favorites work before a session opens.
- **Shared stores and run machinery:** the plan store, edit store and run task hold a `Sql | Command` enum. The events, result store, cancellation and History machinery stay shared.

**Tech Stack:** Rust 1.99, tokio, serde/specta/tauri-specta, rusqlite. UI changes are limited to keeping TypeScript compiling against regenerated bindings.

**Spec:** `docs/superpowers/specs/2026-10-31-non-sql-driver-core-design.md`, §5.4 (backend side), §7.2–§7.7 (backend side), R4 in §8. Also read `docs/superpowers/plans/2026-10-31-r2-followups.md` and `2026-10-31-r3-followups.md` (R4 sections).

## Global Constraints

- **MySQL/PostgreSQL behaviour stays unchanged.** Every existing test passes, except mechanical updates where a wire type gains a field (listed per task). Insta snapshots do not change.
- **The safety promises hold for the Command family:**
  - every command reaches the server only through a guarded plan (`guard_commands` / `policy`);
  - read-only connections never receive writes;
  - Production confirmation names are re-checked at run time (`confirm_names_match`);
  - a data-view read must pass `check_read`;
  - no automatic retry of writes;
  - secrets are redacted in History, logs and Favorites with the connection's `CommandLanguage::redact`.
- **No new crate or npm package.**
- **Identifiers, error text and UI copy are in English.** Exact strings below are verbatim.
- **Bindings:** run `cargo run -p dntdatabase --example export_bindings`; `cargo test -p dntdatabase --test commands bindings_are_current` must pass. The bindings change only as each task lists. TypeScript must keep compiling: run `pnpm --dir ui lint` and `pnpm --dir ui test`, and fix TS minimally. The UI behaviour itself belongs to R4b.
- **`kind_branches` stays green:** exhaustive `match` only.
- **Verification per task:** `cargo check --workspace --all-targets`, the task's tests, `kind_branches`, `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`. When bindings change, also run the UI lint and test.
- **Tests use `FakeCommandDriver::with_kind(DbKind::Redis)`,** registered under `DbKind::Redis` in a test `SessionManager`/`AppState` (Task 1). `src-tauri/tests/command_sessions.rs` (`fixture`, `command_fixture`) is the template.
- **Commit style:** `feat(app|core|store|testkit): …`, one commit per task.
- **Decisions this plan makes:**
  1. `Driver::command_language()` gives a language without a session.
  2. `ParsedCommand.text` holds the display text.
  3. A Command tab switches db by opening a fresh session on that db, never by guessing the language's switch command.
  4. Manual transaction mode and the `query_tx` Commit/Rollback buttons are refused for the Command family (`Unsupported { feature: "transactions" }`); scripts use the language's own `MULTI`/`EXEC`.
  5. `PlanView.dialect` becomes `Option<Dialect>` (`None` for Command), and `PlanView` gains `family: EngineFamily`.
  6. `ProtectionKind` gains `Key`: a Command edit is guarded by its key only.
  7. Schema-cache warm-up and SQL completion are skipped for the Command family (completion returns empty).

## Review Focus

1. **A Production Command script whose confirm name changed between plan and run,** for example after `SELECT n` changed the session db through another tab. The run is refused, never executed. Pinned by Task 3 test `command_run_rechecks_confirm_names`.
2. **`CONFIG SET requirepass hunter2` typed in a Command console.** The History row and every log line contain `***`, never `hunter2`. Pinned by Task 3 test `command_history_is_redacted`.
3. **A data-view page request whose driver returns a write command, or a filter with `$where`.** It is refused before anything is sent. Pinned by Task 5 tests `data_page_refuses_non_read_command` and `raw_json_blocks_server_side_js`.
4. **A key edit on a read-only Redis connection.** Preview gives a Block verdict and apply sends nothing. Pinned by Task 7 test `key_edit_blocked_read_only`.
5. **Saving a Favorite containing `AUTH s3cret` for a Redis connection.** The save is refused, as SQL secrets already are. Pinned by Task 8 test `favorite_with_redis_secret_is_refused`.

---

### Task 1: Driver languages, real-kind fake, available engines

**Files:**
- Modify: `crates/core/src/driver.rs` (`Driver::command_language`) and `crates/core/src/command.rs` (`ParsedCommand.text`)
- Modify: `crates/testkit/src/fake_command.rs` (`with_kind`, `text`, `command_language`)
- Modify: `src-tauri/src/session_manager.rs` (`language_for`, `available_engines`)
- Modify: `src-tauri/src/commands/connections.rs` and `src-tauri/src/commands/mod.rs` (new command)
- Modify: `src-tauri/src/session_manager.rs` (no schema warm-up for the Command family)
- Modify: `ui/src/bindings.ts` (regenerated)

**Interfaces:**
- Produces:

```rust
// Driver (core): default None — SQL drivers keep the default.
fn command_language(&self) -> Option<Arc<dyn CommandLanguage>> { None }
// ParsedCommand (core) gains:
pub text: String,   // the command as written (driver-built commands: the driver's canonical text); never redacted
// testkit:
impl FakeCommandDriver { pub fn with_kind(kind: DbKind) -> Self }   // new() keeps DbKind::Postgres for existing tests
// SessionManager:
pub fn language_for(&self, kind: DbKind) -> Option<Arc<dyn CommandLanguage>>;   // via the registered driver
pub fn available_engines(&self) -> Vec<DbKind>;                                  // kinds with a registered driver, in DbKind order
```

- The new Tauri command is `available_engines() -> Vec<DbKind>`, delegating to `SessionManager::available_engines`.
- Open path: the schema-cache warm-up (`spawn_schema_load`) runs only when the session family is `Sql`, so a Command open logs no warning.
- `with_kind(k)` sets both `Driver::kind()` and `ServerInfo.kind` to `k` (variant `Redis` for Redis, `Mongo` for Mongo).

- [x] **Step 1: Write the failing tests.**
  - `available_engines_lists_registered_drivers`: Postgres and Redis registered gives `[Postgres, Redis]`.
  - `language_for_redis_comes_from_the_driver`: `language_for(Redis)` is `Some`; `language_for(MySql)` with the MySQL driver registered is `None`.
  - `command_open_skips_schema_warmup`: opening a Redis fake connection gives no schema-load attempt. Count `list_objects` calls on the fake, or assert no `Unsupported("schema cache")` warning path ran.
  - `fake_with_kind_reports_redis`
- [x] **Step 2: Run** them. Expected: FAIL.
- [x] **Step 3: Implement.** Register the command and regenerate the bindings. The bindings gain only `availableEngines`.
- [x] **Step 4: Run** `cargo test --workspace`, `bindings_are_current` and the UI lint. Expected: PASS.
- [x] **Step 5: Commit** `feat(core): driver command languages, available engines`.

### Task 2: Plan Command scripts in a query tab

**Files:**
- Modify: `src-tauri/src/query_sessions.rs` (`query_plan`, `session_facts` callers, `StoredPlan`/`PlanCopy`/`PlanStore`, `query_plan_statement`, `ensure_tab_session`, `query_tx_inner`, `query_tx_mode`)
- Modify: `src-tauri/src/commands/query.rs` (`query_plan_impl` gate, `statement_ranges_impl`)
- Modify: `ui/src/bindings.ts` and the TS that reads `PlanView.dialect` (minimal null handling)

**Interfaces:**
- Produces:

```rust
pub enum StoredScript { Sql(ScriptPlan), Command(CommandPlan) }   // in PlanCopy/StoredPlan; insert keeps the guard.is_some() invariant for both
pub struct PlanView { /* … */ pub dialect: Option<Dialect>, pub family: EngineFamily, /* … */ }
```

- **`query_plan_impl`:** `require_sql` is replaced by a family gate. Both families pass; a kind with no registered driver gives the existing "no driver registered" error.
- **`query_plan` on `Engine::Command(c)`:**
  - `plan_commands(text, target, c.language(), c.current_database().as_deref())`;
  - `guard_commands(&mut p, &PolicyCtx { env: cfg.env_tag, read_only: cfg.read_only, confirm_unbounded: <same setting as guard_ctx>, connection_name: cfg.name.clone() })`;
  - `PlanStmtView` from `PlannedCommand`: `kind = label`, `verdict`, `preview(text)`, `confirm_name`, `range`;
  - `unbounded`, `drops` and `estimate` are empty or `None`; `implicit_commit` is `false`;
  - `tx_state` is `dyn Session::tx_state()`.
- **Command tab db switching:** when the tab's db differs from the requested db, close the session and open a fresh one with `with_database(target)`.
- **`query_plan_statement`** (Retry and Fetch all) on a stored `Command` plan builds a one-command plan with the same guard.
- **`statement_ranges_impl`:** for a connection whose kind has a `language_for`, it returns UTF-16 ranges of `language.split(text)`, using the same `Utf16Map` as `plan_commands`. The signature gains the connection id, or the kind, as needed; update the UI caller.
- **Command family refusals:** `query_tx` (Commit/Rollback) and `query_tx_mode(Manual)` return `DbError::Unsupported { feature: "transactions".into(), version: String::new() }`. `query_tx_mode(Auto)` is accepted.
- **Completion:** `completion_scope_impl` and `completion_objects_impl` return an empty result for the Command family instead of an error.

- [x] **Step 1: Write the failing tests** in `src-tauri/tests/command_tabs.rs` (new):
  - `command_plan_has_labels_and_verdicts`: `SET a 1\nGET a\nNUKE` on Production gives labels `write, read, admin`; verdicts `Confirm, Allow, Confirm`; `confirm_name` of stmt 2 is the connection name; `family == Command`; `dialect == None`.
  - `command_plan_blocks_on_read_only`
  - `command_tab_switches_db_with_a_fresh_session`: plan with db "0", then with db "2". The fake counts opens; `current_database()` is "2".
  - `command_statement_ranges_follow_the_language`
  - `command_tx_buttons_are_refused`
  - `sql_plan_view_unchanged`: an existing SQL plan test asserts `dialect == Some(..)` and `family == Sql`.
- [x] **Step 2: Run** them. Expected: FAIL.
- [x] **Step 3: Implement.** Regenerate the bindings; the only changes are `PlanView.dialect` becoming nullable and `PlanView.family`. Fix the TS readers.
- [x] **Step 4: Run** `cargo test -p dntdatabase`, `bindings_are_current`, `pnpm --dir ui lint` and `pnpm --dir ui test`. Expected: PASS.
- [x] **Step 5: Commit** `feat(app): plan Command scripts in query tabs`.

### Task 3: Run Command scripts (events, History, redaction)

**Files:**
- Modify: `src-tauri/src/query_sessions.rs` (`query_run_named`, `rejudge`, `RunTask`, `after_run`)
- Modify: `src-tauri/src/run_events.rs` (`ChannelSink` implements `CommandRunSink`; redaction via the language)

**Interfaces:**
- **`rejudge`** for a `Command` plan reruns `guard_commands` with a fresh `PolicyCtx` built from the current config. It then applies the same Block / Confirm / `confirm_names_match` rules as SQL, with identical error kinds and messages.
- **`RunTask::execute`** on `Command` calls `dnt_query::command::run_commands(c, &plan, &exec, RunConfig { continue_on_error, manual_tx: false }, &cancel, &mut sink)`. Both families share the after-run handling.
- **`ChannelSink`** holds `redactor: Redactor`, defined as `enum Redactor { Sql(Dialect), Command(Arc<dyn CommandLanguage>) }`.
  - `statement_start` / `statement_end` for `PlannedCommand` emit the same `RunEvent`s as SQL.
  - History rows are `origin: Query`, `kind` = the `StmtKindLabel` string, and `sql_text` = the `redact`ed text.
  - `CommandRunSink::database(name)` sets the tab's db, as `use_db` does for SQL.
  - DDL-success refresh: `CommandClass::Ddl` triggers the same tree refresh as SQL DDL, but no schema cache.
- **`after_run`:** the tx-fresh and tx-tracking logic is skipped for Command plans, and the tab's tx state comes from `dyn Session::tx_state()`.

- [ ] **Step 1: Write the failing tests** in `command_tabs.rs`. Collect events through the existing EventSink test pattern in `src-tauri/tests/query_sessions.rs`.
  - `command_run_streams_rows_and_history`: `SET a 1\nGET a` gives events `StatementStart`, `ResultEnd`, …, then `Done{stop: None}`. The rows for `GET` contain `"1"`. There are 2 History rows, origin `query`, kinds `write` and `read`.
  - `command_history_is_redacted`: `CONFIG SET requirepass hunter2` run on Local gives a History `sql_text` containing `***` and not `hunter2`.
  - `command_run_rechecks_confirm_names`: a Production plan for `FLUSHDB`, run with the right name, executes. A second plan run with a wrong name gives the same error SQL gives for a wrong confirm name, and nothing reaches the fake (`store.executed()` is unchanged).
  - `command_run_blocked_plan_is_refused`
  - `command_run_reports_database_change`: `SELECT 2` sets the tab db to "2", so the next plan uses db "2".
  - `command_cancel_stops_a_blocked_command`: `block_next`, then `query_cancel`, gives `Done{stop: Cancelled}`.
- [ ] **Step 2: Run** them. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p dntdatabase`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(app): run Command scripts with events and redacted History`.

### Task 4: Browse a Command connection

**Files:**
- Modify: `src-tauri/src/commands/browse.rs` (`list_databases_impl`, `list_schemas_impl`)

**Interfaces:**
- The two `require_sql` calls become an exhaustive match on `cfg.kind.family()`.
  - **Sql:** today's code with `filter_system`.
  - **Command:** the Introspector result is passed through unfiltered; `list_schemas` returns it as is, normally empty.
- `list_objects`, `table_detail`, `object_ddl` and `refresh_node` already work; this task changes only the fake.
- Fake convention, documented in `fake_command.rs` and also used by Tasks 5 and 6:
  - every hash key `h:<name>` in a db is listed by `list_objects(scope, Collection)` as a Collection named `<name>`;
  - `table_detail` of that collection gives column `_id` (the field name) plus `value`, with `_id` as primary key;
  - its rows are the hash's fields.

- [ ] **Step 1: Write the failing tests:**
  - `command_tree_lists_databases_and_objects`: the fake's dbs with keys plus the current db; `list_objects(Collection)` gives `["users"]` after `HSET h:users u1 alice`; `table_detail` of it gives the two columns with `_id` primary.
  - `sql_tree_unchanged`
- [ ] **Step 2: Run** them. Expected: FAIL on `list_databases`.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p dntdatabase`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(app): browse Command connections`.

### Task 5: Data-view reads for the Command family

**Files:**
- Modify: `crates/core/src/sql/filter.rs` (`DataFilter.raw_json`)
- Create or modify: `crates/core/src/data_access.rs` (`check_raw_json`)
- Modify: `src-tauri/src/commands/data.rs` (`table_data_impl`, `table_row_count_impl`, `check_request_size`)
- Modify: `src-tauri/src/data_sessions.rs` (`DataRequest` command variant)
- Modify: `crates/testkit/src/fake_command.rs`: `DataAccess::page` and `count` for the Task 4 collection convention. Rows are `(_id = field, value)`, sorted by `_id`. `skip`/`limit` are honoured. Structured filters support `Eq` on `_id`/`value`; any other op gives `Unsupported`.
- Modify: `ui/src/bindings.ts` (regenerated) and the TS `DataFilter` literals (`raw_json: null`)

**Interfaces:**
- Produces:

```rust
// DataFilter gains (serde default):
pub raw_json: Option<String>,
// data_access.rs:
pub fn check_raw_json(text: &str) -> Result<(), DbError>;
// must parse as a JSON object; any key "$where", "$function" or "$accumulator" at any depth →
// Config { field: Some("filter"), message: "Server-side JavaScript is not allowed in a filter" }; not an object →
// Config { field: Some("filter"), message: "The filter must be a JSON object" }
```

- **`check_request_size`:**
  - `raw_where` and `raw_json` together give `Config { field: "filter", message: "Use either a WHERE filter or a JSON filter, not both" }`.
  - `raw_json` on the SQL family, or `raw_where` on the Command family, gives `Config { field: "filter", message: "This filter type is not available for this engine" }`.
  - The size limit applies to `raw_json` as it does to `raw_where`.
- **Command path of `table_data_impl` / `table_row_count_impl`:**
  - Requires `caps.family == Command`.
  - Runs `check_raw_json` when present.
  - Builds `c.data_access().page(&obj, &PageSpec { sort, filter, limit: limit + 1, skip: offset })` or `.count(..)`, then `check_read(&cmd)?`.
  - Runs the command through the data-session pool. Add `DataRequest::Command(ParsedCommand)` and `execute_command` with the same pooling, cancel and supersede rules.
  - Splits the page the same way. `row_versions`, `hidden_key` and `hidden_key_values` are `None` or empty, and `key_order` is the table detail's key, `["_id"]` when known.
  - The count reads the first cell, as SQL does.

- [ ] **Step 1: Write the failing tests:**
  - `data_page_reads_through_data_access`
  - `data_count_reads_through_data_access`
  - `data_page_refuses_non_read_command`: a fake hook makes `page` return a Write command, giving `Internal "the driver built …"`; nothing is executed.
  - `raw_json_blocks_server_side_js`: `{"a":{"$where":"1"}}` gives the exact message.
  - `raw_json_must_be_an_object`
  - `filter_types_are_per_family`
  - `sql_data_view_unchanged`
- [ ] **Step 2: Run** them. Expected: FAIL.
- [ ] **Step 3: Implement.** Regenerate the bindings; they may only gain `DataFilter.raw_json`. Fix the TS literals.
- [ ] **Step 4: Run** `cargo test --workspace`, `bindings_are_current`, the UI lint and the UI test. Expected: PASS.
- [ ] **Step 5: Commit** `feat(app): Command data-view reads via DataAccess`.

### Task 6: Data-view edits for the Command family

**Files:**
- Modify: `src-tauri/src/commands/data.rs` (`data_editable_impl`, `data_preview_impl`, `data_apply_impl`)
- Modify: `src-tauri/src/edit_plans.rs` (stored edit enum, Command preview and apply, History)
- Modify: `crates/query/src/edit.rs` (`ProtectionKind::Key`)
- Modify: `crates/testkit/src/fake_command.rs`: `build_edits` for the collection convention from Task 5. Set a field, delete a field, or add a row.
- Modify: `ui/src/bindings.ts` (regenerated) and any TS `Record<ProtectionKind, …>`

**Interfaces:**
- **`data_editable_impl`, Command family:**
  - Editable when the connection is not read-only, `caps.data_edit` is true and `obj.kind == Collection`.
  - Then: `key: ["_id"]` (or the table detail's key), `editable_columns` = all non-key columns, `atomic: if caps.transactions { Yes } else { No }`, `protection: ProtectionKind::Key`.
  - Otherwise `editable: false` with a reason: `the connection is read-only`, `editing is not available for this engine`, or `only collections can be edited`.
- **`data_preview_impl`, Command family:**
  - `cmds = c.data_access().build_edits(&obj, &detail, &set)?`.
  - Each command's verdict comes from `policy(&RiskFacts::from(cmd), &ctx)`; the set's verdict is the strictest.
  - `EditPreview.statements` are `cmd.text`, `atomic` as above, and `protections` are `Key`.
  - Stored as `StoredEdit::Command { commands, … }`.
- **`data_apply_impl`, Command family:**
  - Rechecks the verdict against the current config, as SQL does.
  - Runs each command in order through a data session with `execute_command`.
  - Stops at the first error and reports per command in `ApplyReport`.
  - Writes History rows: `origin: DataEdit`, text redacted with the language.
  - No retry.
- **Production:** the same rule as SQL for non-atomic edits. When `atomic != Yes`, the edit is blocked with the existing `NOT_ATOMIC_ON_PRODUCTION`-style message, adapted: `Changes to this collection cannot be rolled back as a unit, so the data view does not edit it on a Production connection. Use the console instead.`

- [ ] **Step 1: Write the failing tests:**
  - `command_collection_is_editable`
  - `command_edit_preview_lists_commands`
  - `command_edit_apply_writes_and_logs_history`
  - `command_edit_blocked_on_production_when_not_atomic`
  - `command_edit_refused_read_only`
  - `sql_edit_unchanged`
- [ ] **Step 2: Run** them. Expected: FAIL.
- [ ] **Step 3: Implement.** Regenerate the bindings; they may only gain `ProtectionKind` `"key"` (serde casing as existing).
- [ ] **Step 4: Run** `cargo test --workspace`, `bindings_are_current`, the UI lint and the UI test. Expected: PASS.
- [ ] **Step 5: Commit** `feat(app): Command data-view edits`.

### Task 7: Key browser backend

**Files:**
- Create: `src-tauri/src/commands/keys.rs`
- Modify: `src-tauri/src/commands/mod.rs` (registration)
- Modify: `src-tauri/src/session_manager.rs` or `src-tauri/src/edit_plans.rs` (a session on the requested db, plus the edit store for key edits)
- Modify: `crates/core/src/keyspace.rs` (`#[specta(type = f64)]` on `u64`/`i64` fields)
- Modify: `ui/src/bindings.ts` (regenerated)

**Interfaces:**
- Produces these Tauri commands (each with an `_impl`):

```rust
keys_scan(id: Uuid, db: String, req: ScanRequest) -> KeyPage                       // require caps.paged_objects
key_value(id: Uuid, db: String, key: KeyName, range: ValueRange) -> KeyValueView    // require caps.paged_objects
key_edit_preview(id: Uuid, db: String, edit: KeyEdit) -> EditPreview                // require caps.data_edit
// apply reuses data_apply(plan_id, confirmed) through the shared edit store
```

- **Caps gate:** missing caps give `Unsupported { feature: "key browser" }` or `"data editing"`.
- **Scan and value** run on a data session for `db`, via `Session::keyspace()`. `None` gives `Unsupported { feature: "key browser" }`.
- **`key_edit_preview`:** `build_key_edit(db, &edit)`, then the per-command policy verdict exactly as in Task 6, stored the same way. Read-only gives Block with the reason `the connection is read-only`.
- **`count` on `ScanRequest`** is clamped to 1..=1000.
- **Bindings:** the keyspace and `KeyEdit` types are exported because these commands use them. The bindings gain only the new commands and the types they reference.

- [ ] **Step 1: Write the failing tests:**
  - `keys_scan_pages_through_keys`
  - `key_value_reads_a_hash`
  - `key_edit_round_trip`: preview, then `data_apply`, then the value has changed, and a History row is written.
  - `key_edit_blocked_read_only`
  - `key_edit_confirms_on_production`: `Delete` on Production gives Confirm.
  - `keys_refused_without_paged_objects`: a fake with `paged_objects` off.
- [ ] **Step 2: Run** them. Expected: FAIL.
- [ ] **Step 3: Implement.** Register the commands and regenerate the bindings.
- [ ] **Step 4: Run** `cargo test --workspace`, `bindings_are_current`, the UI lint and the UI test. Expected: PASS.
- [ ] **Step 5: Commit** `feat(app): key browser commands`.

### Task 8: Favorites redact with the connection's language

**Files:**
- Modify: `crates/store/src/favorites.rs` (`refuse_secrets`, `validate_favorite`, `favorite_create`/`favorite_update` signatures)
- Modify: `src-tauri/src/commands/favorites.rs`

**Interfaces:**
- `favorite_create` and `favorite_update` take `redact: &dyn Fn(&str) -> Redacted` instead of hard-coding the SQL dialects.
- The command layer chooses the redactor:
  - when the favorite has a `connection_id` whose kind has `language_for`, use that language's `redact`;
  - otherwise use today's SQL check (both dialects).
- The refusal error is unchanged.

- [ ] **Step 1: Write the failing tests:**
  - `favorite_with_redis_secret_is_refused`: `AUTH s3cret` with a Redis connection id gives the existing refusal error.
  - `favorite_without_connection_uses_sql_check`
  - `sql_favorite_rules_unchanged`
- [ ] **Step 2: Run** them. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p dnt-store` and `cargo test -p dntdatabase`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(app): Favorites redact with the connection's language`.

### Task 9: Full verification

- [ ] **Step 1: Run the CI set:** `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `pnpm --dir ui lint` and `pnpm --dir ui test`. Expected: PASS.
- [ ] **Step 2: Run the DB suite** against the user's running `dnt-test` stack, without compose up/down. Swap in its certs/keys from `D:\DntDatabase\testdata`, then restore this worktree's own, verifying by hash. Run with `DNT_TEST_DB=1` and `--test-threads=1`. Expected: PASS, apart from the known MySQL cancel flakes, which are fixed on unmerged branches.
- [ ] **Step 3: Run e2e-web:** `$env:E2E_WEB_PORT='5176'; pnpm --dir e2e-web test`. Expected: PASS. e2e-web uses a fake IPC backend, so new commands it does not mock may need minimal mocks; list them.
- [ ] **Step 4: Commit** only if a step required a change.
