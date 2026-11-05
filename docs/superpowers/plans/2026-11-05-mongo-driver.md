# MongoDB driver Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A real MongoDB driver (`crates/driver-mongo`), registered in `default_drivers()`, so Mongo connections work end to end with the R4a backend and the R4b UI. This includes the core/app changes in spec §9.

**Architecture:**
- **Client:** the official `mongodb` 3.x crate, rustls with ring, plus `dns-resolver`. One `Client` per session; it can be rebuilt after a cancel.
- **Console:** a closed mongosh-subset grammar. It classifies each command itself, without asking the server, and accepts mongosh helpers and Extended JSON v2 literals.
- **Execution:** results are buffered so each result emits a single column header. A read-only allowlist sits in the executor, independent of the classifier.
- **Atomic edits:** a new core `CommandSession::execute_atomic`, which the backend calls for atomic plans.

**Tech Stack:** Rust 1.99, tokio, `mongodb` 3.x (`compat-3-0-0`, `rustls-tls`, `dns-resolver`) and its `mongodb::bson` re-export, async-trait, secrecy. React/TypeScript for one UI change. Docker Compose for the MongoDB 6.0 standalone and 8.0 replica-set services.

**Spec:** `docs/superpowers/specs/2026-11-05-mongo-driver-design.md` (rev 2). Read it together with the core spec `2026-10-31-non-sql-driver-core-design.md`. Section numbers below (§n) refer to the Mongo spec.

## Global Constraints

- **MySQL, PostgreSQL and Redis behaviour is unchanged.** Existing tests pass and Insta snapshots do not change.
- **TLS:** rustls with ring only. `Cargo.lock` must contain none of `aws-lc-rs`, `aws-lc-sys`, `openssl-sys` or `native-tls`. Check `cargo tree -i` for each of these after adding `mongodb`.
- **Dependencies:** the only new crate is `mongodb`. Use `mongodb::bson`; do not add a separate `bson` dependency.
- **Copy:** identifiers, error text and UI copy are in English. Exact strings come from the spec, verbatim.
- **Safety promises:**
  - Commands reach the server only through a guarded plan.
  - A read-only connection never receives a write. The executor's allowlist (§5.3) is a second, independent layer.
  - No automatic retry. The only exception is commit/abort in `execute_atomic` (§6.4).
  - Passwords never appear in a URI, a temp file, History or logs.
- **Kind branches:** Rust uses exhaustive `match` only (`kind_branches` stays green). The UI keeps kind literals inside `ui/src/engine/`.
- **Bindings:** the generated bindings change only where a task says so. Regenerate with `cargo run -p dntdatabase --example export_bindings`, then `bindings_are_current` must pass.
- **Verification per task:**
  - Always: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, and the task's tests.
  - UI tasks also run `pnpm --dir ui lint` and `pnpm --dir ui test`.
  - DB tests run with `DNT_TEST_DB=1` and `--test-threads=1`, against the compose services the task names. Start only those services; never tear down the user's running stack.
- **Files:** UTF-8 without BOM (`no_bom`).
- **Commits:** one per task, style `feat(core|app|mongo|ui|testkit): …`, each ending with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Review Focus

1. **`aggregate` with `$out` nested inside `$facet` or `$lookup.pipeline` on a read-only connection.** The command is refused before it is sent, at both layers. Pinned by Task 7 `out_in_nested_pipeline_is_write` and Task 9 `read_only_allowlist_ignores_class`.
2. **`db.adminCommand({dropDatabase: 1})` on Production.** The confirm dialog asks for `admin`, not the current db. Pinned by Task 7 `admin_command_drop_names_admin` and Task 13 `guard_names_admin_for_admin_drop`.
3. **Editing a nested JSON cell that holds an ObjectId, a Date and a Long.** The saved document keeps all three types. Pinned by Task 5 `nested_types_round_trip_canonical_ejson` and Task 12 `nested_json_edit_keeps_types`.
4. **A replica-set apply whose second command fails.** Nothing is written, and the report says rolled back. Pinned by Task 12 `atomic_apply_rolls_back`.
5. **A `find` whose later batches add fields.** The data view keeps every row, and the console shows one result with all columns. Pinned by Task 9 `new_fields_in_later_batches_one_header`.

---

### Task 1: Core `execute_atomic`, hints, and the fake driver

**Files:**
- Modify: `crates/core/src/command.rs`, which gains `execute_atomic` and `AtomicFailure`.
- Modify: `crates/core/src/error.rs`, which gains `static_hint` entries.
- Modify: `crates/testkit/src/fake_command.rs`, which implements `execute_atomic`.

**Interfaces:**
- Produces:

```rust
// CommandSession (default method):
async fn execute_atomic(&self, cmds: &[ParsedCommand], opts: &ExecOptions)
    -> Result<Vec<StmtOutcome>, AtomicFailure> {
    Err(AtomicFailure { index: 0, error: DbError::Unsupported { feature: "transactions".into(), version: String::new() }, rolled_back: false })
}
pub struct AtomicFailure { pub index: usize, pub error: DbError, pub rolled_back: bool }
```

- `static_hint` maps these server codes to hints:

| Code | Hint |
|---|---|
| `Unauthorized` | `The user lacks the role for this command` |
| `DuplicateKey` | `A unique index rejected a duplicate value` |
| `NotWritablePrimary` | `Connected to a secondary; writes need the primary` |

- The fake's `execute_atomic` runs the commands in order on a snapshot of its store. On the first error it restores the snapshot and returns `rolled_back: true`. A test hook `fail_atomic_at(index)` forces a failure at a given command.

- [ ] **Step 1: Write the failing tests:**
  - `default_execute_atomic_is_unsupported`
  - `hints_for_mongo_codes`
  - `fake_execute_atomic_commits`
  - `fake_execute_atomic_rolls_back`
- [ ] **Step 2: Run** `cargo test -p dnt-core -p dnt-testkit`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --workspace`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(core): CommandSession::execute_atomic and Mongo error hints`.

### Task 2: Backend apply, fingerprint, and display text

**Files:**
- Modify: `src-tauri/src/edit_plans.rs`. The changes are in `command_edit_preview` (around line 1585) and `command_edit_apply` (around line 1731).
- Modify: `crates/query/src/edit.rs`, which gains `command_fingerprint` and `display_command`.
- Test: `src-tauri/tests/command_data.rs`

**Interfaces:**
- Consumes: Task 1.
- Produces:

```rust
pub fn command_fingerprint(detail: &TableDetail) -> u64;   // hashes the key columns' db_type and the index list only
pub fn display_command(text: &str) -> (String, bool);      // keeps 4 KiB, appends "…"; bool = shortened
```

- **Apply.** When the plan's `atomic == Yes`, `command_edit_apply` calls `execute_atomic` once.
  - `Err(AtomicFailure { rolled_back: true, .. })` maps to `ApplyOutcome::RolledBack`.
  - An error with `code == Some("DocumentNotFound")` sets `conflict`.
  - Otherwise apply keeps today's per-command loop.
- **Fingerprint.** Preview and apply use `command_fingerprint` for the Command family. The SQL family keeps `fingerprint`.
- **Display text.** Preview statements and History rows store `display_command(cmd.text)`. A shortened row is marked redacted, like `display_sql`.

- [ ] **Step 1: Write the failing tests:**
  - `atomic_plan_uses_execute_atomic`: the fake counts the call, and no per-command execute happens.
  - `atomic_failure_is_rolled_back`: uses `fail_atomic_at(1)`. The outcome is `RolledBack`, and the store is unchanged.
  - `document_not_found_sets_conflict`
  - `command_fingerprint_ignores_sampled_columns`: adding a non-key column does not change the fingerprint; changing the `_id` type does.
  - `long_command_text_is_shortened_in_history`
  - `sql_apply_unchanged`
- [ ] **Step 2: Run** `cargo test -p dntdatabase --test command_data`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --workspace`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(app): atomic Command edits, Command fingerprint, shortened command text`.

### Task 3: Raw filter, system objects, and Mongo URI options

**Files:**
- Modify: `src-tauri/src/commands/data.rs`, where the `check_raw_json` call is around line 740.
- Modify: `src-tauri/src/commands/browse.rs`: `list_databases_impl` (around line 121), `list_objects_impl` (around line 163), and `filter_system` (around line 80).
- Modify: `crates/core/src/conn_string.rs` (Mongo option handling, around line 459).
- Test: `src-tauri/tests/command_data.rs`, `src-tauri/tests/command_tabs.rs` (or the browse tests), and the `conn_string` unit tests.

**Interfaces:**
- **Raw filter.** The Command family no longer calls `check_raw_json` in the app; the driver validates the filter itself (§6.2). The size limit and the "JSON filter vs WHERE filter" rules stay as they are.
- **System objects.** Add a new function:

  ```rust
  pub fn command_system_names(kind: DbKind) -> (&'static [&'static str] /* dbs */, &'static str /* collection prefix */)
  ```

  - Mongo returns `(["admin", "local", "config"], "system.")`. Redis returns `([], "")`.
  - It is an exhaustive match.
  - When `show_system` is false, `list_databases_impl` and `list_objects_impl` filter Command results with it. `list_objects_impl` gains the `show_system` argument the SQL path already uses.
  - The bindings change only if a command signature changes.
- **Mongo URI options.**
  - `retryWrites`, `w`, `appName`, `journal`, `wtimeoutMS`, `maxPoolSize` and `readPreference` are accepted and ignored.
  - A missing `authSource` takes the path database.
  - Other unknown options keep today's error.

- [ ] **Step 1: Write the failing tests:**
  - `command_raw_filter_reaches_driver_unparsed`: the fake receives `{_id: ObjectId("…")}` unchanged.
  - `mongo_system_dbs_hidden_unless_show_system`
  - `mongo_system_collections_hidden`
  - `redis_tree_unchanged`
  - `atlas_uri_options_accepted`
  - `auth_source_defaults_to_path_db`
- [ ] **Step 2: Run** them. Expected: FAIL.
- [ ] **Step 3: Implement.** Regenerate the bindings if a signature changed, and fix the TS callers.
- [ ] **Step 4: Run** `cargo test --workspace`, `bindings_are_current` and the UI lint and tests. Expected: PASS.
- [ ] **Step 5: Commit** `feat(app): Command raw filters reach the driver; Mongo system objects; Atlas URI options`.

### Task 4: Data view and e2e fake for Mongo

**Files:**
- Modify: `ui/src/tabs/DataView.tsx` (or the cell-editing helper it uses).
- Modify: the e2e-web Mongo fake (`e2e-web/src/commandQuery.ts`, `commandData.ts`), plus `e2e-web/specs/command-query.spec.ts` and `command-data.spec.ts`.
- Test: `ui/src/tabs/DataView.test.tsx`

**Interfaces:**
- **Cut cells.** For the Command family, a cell whose value is a preview (cut) has no Edit action, and editing it is not offered (§6.3).
- **Fake rows.** Mongo rows in the fake are flattened by top-level field, `_id` first (spec core §7.3), instead of `{_id, document}`.

- [ ] **Step 1: Write the failing tests:**
  - `command_cut_cell_not_editable`
  - `sql_cut_cell_still_loads`
- [ ] **Step 2: Run** `pnpm --dir ui test DataView`. Expected: FAIL.
- [ ] **Step 3: Implement.** Update the fake and both e2e specs so they expect flattened columns.
- [ ] **Step 4: Run** the UI lint and tests, and `$env:E2E_WEB_PORT='5176'; pnpm --dir e2e-web test specs/command-query.spec.ts specs/command-data.spec.ts`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(ui): Command cut cells are read-only; Mongo fake rows flattened`.

### Task 5: Crate skeleton and value mapping

**Files:**
- Create: `crates/driver-mongo/Cargo.toml`, `crates/driver-mongo/src/lib.rs` (modules only), `crates/driver-mongo/src/rows.rs`
- Modify: root `Cargo.toml` (`mongodb` workspace dependency)

**Interfaces:**
- Produces (`rows.rs`):

```rust
pub fn bson_to_value(b: &Bson, cells: &CellMode) -> Value;                          // §7 left column
pub fn value_to_bson(v: &Value, db_type: Option<&str>) -> Result<Bson, DbError>;    // §7 right column
pub fn canonical_ejson(b: &Bson) -> String;                                          // nested docs/arrays → Value::Json text
pub fn columns_of(docs: &[Document]) -> Vec<String>;                                 // `_id` first, then first appearance
pub fn row_of(doc: &Document, cols: &[String], cells: &CellMode) -> Vec<Value>;      // missing field → Null
```

- **Dependency:** `mongodb = { version = "3", default-features = false, features = ["compat-3-0-0", "rustls-tls", "dns-resolver"] }`.
- **`value_to_bson` errors** are `Config { field: None, message: "<type> values are read-only" }`. They apply to the read-only kinds: `javascript`, `codeWithScope` and `dbPointer`.

- [ ] **Step 1: Write the failing tests:**
  - `every_bson_kind_maps` (each row of §7)
  - `binary_keeps_subtype_and_bytes`
  - `nan_and_out_of_range_dates_are_other`
  - `nested_types_round_trip_canonical_ejson` (ObjectId, Date, Int64 and Decimal inside a nested doc, through `canonical_ejson`, then back through Task 6's parser; mark it `#[ignore]` until Task 6, and Task 6 un-ignores it)
  - `int_kind_follows_db_type`
  - `columns_id_first_then_appearance`
- [ ] **Step 2: Run** `cargo test -p dnt-driver-mongo rows`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the tests, then `cargo tree -i aws-lc-rs`, `cargo tree -i aws-lc-sys`, `cargo tree -i openssl-sys` and `cargo tree -i native-tls`. Expected: the tests PASS, and every `cargo tree` reports the package not found.
- [ ] **Step 5: Commit** `feat(mongo): crate skeleton and BSON value mapping`.

### Task 6: Shell lexer and literal parser

**Files:**
- Create: `crates/driver-mongo/src/shell/{mod.rs, lexer.rs, literal.rs}`

**Interfaces:**
- Produces:

```rust
pub struct Token { pub kind: TokenKind, pub start: usize, pub end: usize }   // byte offsets
pub fn lex(text: &str) -> Result<Vec<Token>, ShellError>;
pub fn parse_literal(tokens: &[Token], text: &str, at: &mut usize) -> Result<Bson, ShellError>;
pub fn parse_document(text: &str) -> Result<Document, ShellError>;   // whole text must be one document (raw filter, JSON cells)
pub struct ShellError { pub message: String, pub offset: usize }
```

- The parser covers every literal, helper and Extended JSON v2 wrapper in §4.3, with mongosh number rules.
- A malformed wrapper (for example `{"$oid": 5}`) is an error, not a sub-document.

- [ ] **Step 1: Write the failing tests:**
  - `js_object_literals`
  - `numbers_follow_mongosh`
  - `helpers_with_and_without_args`
  - `extended_json_canonical_and_relaxed` (every wrapper)
  - `bad_wrapper_is_error`
  - `regex_literal`
  - `errors_have_offsets`
  - Un-ignore `nested_types_round_trip_canonical_ejson`.
- [ ] **Step 2: Run** `cargo test -p dnt-driver-mongo shell`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p dnt-driver-mongo`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(mongo): shell lexer and literal parser with Extended JSON`.

### Task 7: Grammar, split, classify, and redact (`MongoLanguage`)

**Files:**
- Create: `crates/driver-mongo/src/shell/{parse.rs, classify.rs, redact.rs}`, and extend `shell/mod.rs` with `MongoLanguage`.

**Interfaces:**
- Produces:

```rust
pub enum ShellCommand {
    Use(String), Show(ShowWhat),
    Db { db: Option<String>, method: DbMethod, args: Vec<Bson>, modifiers: Vec<Modifier> },
    Coll { db: Option<String>, coll: String, method: CollMethod, args: Vec<Bson>, modifiers: Vec<Modifier>,
           explain: Option<Verbosity> },
}
pub fn has_out_or_merge(pipeline: &[Bson]) -> Option<(OutKind, CommandTarget)>;   // §4.4.2, recursive
pub struct MongoLanguage;    // CommandLanguage: split §4.1, parse §4.2–4.4, redact §4.5
pub(crate) struct Payload { pub cmd: ShellCommand }   // ParsedCommand.payload
```

- **Grammar, classification, and runCommand table:** follow spec §4.1–§4.4.1 exactly, including the error strings.
- **Targets, drops and `unbounded`:** follow §4.4 and §4.4.1, and use the "db of the command" rule.
- **Redact:** follows §4.5, with a keyword and URI fallback when parsing fails.

- [ ] **Step 1: Write the failing tests:**
  - `split_multiline_and_chained_lines`
  - `split_ignores_semicolons_in_strings_and_regex`
  - `every_stmt_form_parses`
  - `dotted_collection_names`
  - `db_method_vs_collection`
  - `explain_both_positions`
  - `explain_verbosity_refused_for_writes`
  - `misplaced_modifier_is_error`
  - `javascript_is_refused` (`var`, `let`, `const`, `=>`, `function`, `load(`, `eval(`, `.forEach(`, `.map(`, `=`, `+`)
  - `invalid_db_name`
  - `classification_table` (every row of §4.4)
  - `run_command_table` (every row of §4.4.1)
  - `out_in_nested_pipeline_is_write` (inside `$facet`, `$lookup.pipeline` and `$unionWith.pipeline`)
  - `out_drops_destination`
  - `rename_drop_target_drops`
  - `admin_command_drop_names_admin`
  - `sibling_db_drop_names_it`
  - `unbounded_update_many_and_run_command_delete`
  - `validate_is_admin`
  - `redacts_password_keys_at_any_depth`
  - `redacts_uri_password_with_at_sign`
  - `redacts_unparsable_lines`
  - `split_never_panics` (a fuzz-ish table of broken inputs)
- [ ] **Step 2: Run** `cargo test -p dnt-driver-mongo shell`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p dnt-driver-mongo`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(mongo): console grammar, classification and redaction`.

### Task 8: Connect, TLS, and test services

**Files:**
- Create: `crates/driver-mongo/src/client.rs`
- Modify: `crates/driver-mongo/src/lib.rs` (`MongoDriver`; `connect` returns the Task 9 session; until Task 9, tests call `client::open` directly)
- Modify: `testdata/docker-compose.test.yml` (services `mongo60` and `mongo80`)
- Create: `testdata/mongo/` (keyfile generator note, init script, seed JSON)
- Modify: `crates/testkit/src/lib.rs` (`Target::Mongo60` / `Target::Mongo80`, ports `27060` / `27080`, `resolved_as` for Mongo)
- Modify: `crates/testkit/src/bin/gen_fixtures.rs` (generate the keyfile and the combined client PEM)
- Test: `crates/driver-mongo/tests/connect.rs`, plus unit tests in `client.rs`

**Interfaces:**
- Produces:

```rust
pub(crate) struct Opened { pub client: Client, pub info: ServerInfo, pub db: String }
pub(crate) async fn open(cfg: &ResolvedConfig) -> Result<Opened, DbError>;
pub(crate) fn client_options(cfg: &ResolvedConfig, through_tunnel: bool) -> Result<ClientOptionsPlan, DbError>;  // pure; unit-testable
```

- **`ClientOptions`, TLS, SRV, client-cert checks, version check, errors and capabilities:** follow spec §5.1 exactly, including the error strings and fields.
- **Tunnel detection.** `through_tunnel` is true when `cfg.host` is `127.0.0.1` and `cfg.tls_server_name` differs from it. That is how `SessionManager` marks a tunnel (`resolved_config`). If that inference is ambiguous, add an explicit `ResolvedConfig.via_tunnel: bool` and set it in `session_manager.rs`; record the choice in the report.
- **Compose:**
  - Both services run with `--setParameter enableTestCommands=1`, so the `failCommand` failpoint is available. This is test-only.
  - `mongo80` runs `--replSet rs0 --keyFile /data/keyfile --tlsMode allowTLS --tlsCertificateKeyFile …`, with a healthcheck that runs `rs.initiate` once. It creates the users `root` and `dnt` (§8.2) and seeds `dnt_fixture`.

- [ ] **Step 1: Write the failing tests:**
  - Unit tests:
    - `options_plain`
    - `options_srv_has_no_password_in_uri`
    - `srv_without_tls_is_config_error`
    - `verify_ca_refused`
    - `verify_full_through_tunnel_refused`
    - `cert_must_be_combined_pem`
    - `key_path_refused`
    - `credential_only_with_username`
    - `auth_source_fallbacks`
    - `retries_off`
    - `app_name_from_config`
  - Integration tests (`require_db!`):
    - `connects_standalone_60`
    - `connects_replica_set_tls_80`
    - `wrong_password_is_auth`
    - `unverified_tls`
    - `client_cert_pem`
    - `transactions_capability_follows_topology`
- [ ] **Step 2: Run** them. Expected: FAIL.
- [ ] **Step 3: Implement.** Document the new compose services in the file header.
- [ ] **Step 4: Run** the tests again, plus `cargo test -p dnt-testkit`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(mongo): client options, TLS and connect; MongoDB 6.0 and 8.0 test services`.

### Task 9: Session and execution

**Files:**
- Create: `crates/driver-mongo/src/session.rs`, `crates/driver-mongo/src/exec.rs`
- Test: `crates/driver-mongo/tests/console.rs`, plus unit tests in `exec.rs`

**Interfaces:**
- Produces:

```rust
pub struct MongoSession { client: Arc<RwLock<Client>>, cfg: ResolvedConfig, info: ServerInfo,
                          current_db: Arc<Mutex<String>>, read_only: bool, running: Arc<Mutex<Option<Running>>>,
                          reconnects: AtomicU64 }
pub(crate) struct Running { pub uuid: String, pub class: CommandClass }
pub(crate) fn allowed_read_only(cmd: &ShellCommand) -> bool;     // §5.3 allowlist; never reads ParsedCommand.class
```

- **`Session` and `CommandSession`:**
  - `engine` is `Command(self)`. `keyspace` is `None`. `tx_state` is always `Idle`.
  - `current_database` returns `Some(current_db)`.
  - `ping` and `close` follow §5.2.
- **`execute_command`:**
  1. Downcast the payload.
  2. On a read-only connection, check `allowed_read_only`.
  3. Set `Running` and the comment.
  4. Dispatch by `ShellCommand` according to §5.4.
- **Execution details:** buffering and a single `result_start`, the result shapes, `affected`, the `result` row, `insertMany` partial failure, `maxTimeMS = min`, `use` with the permission check, and `$collStats`, all as in §5.4.
- **Errors:** map as in §5.6. Network errors become `ConnectionLost` and bump `reconnects()`.

- [ ] **Step 1: Write the failing tests:**
  - Unit tests:
    - `read_only_allowlist_ignores_class` (a write `ShellCommand` is refused even when `class` says Read; an aggregate with nested `$out` is refused)
    - `buffer_emits_one_header`
  - Integration tests:
    - `find_sort_limit_project`
    - `new_fields_in_later_batches_one_header` (`batchSize` 2, docs with different fields)
    - `aggregate_out_writes`
    - `writes_report_affected` (`affected` is `matchedCount` for update)
    - `insert_many_partial_failure_message`
    - `use_switches_db_and_checks_permission`
    - `show_dbs_and_collections`
    - `explain_query_planner`
    - `run_command_ping`
    - `collection_stats_uses_collstats_stage`
    - `max_time_ms_is_timeout`
    - `read_only_refuses_insert` (`countDocuments` unchanged)
    - `cells_preview_and_full`
- [ ] **Step 2: Run** them. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p dnt-driver-mongo` with `DNT_TEST_DB=1`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(mongo): session and console execution`.

### Task 10: Cancel

**Files:**
- Create: `crates/driver-mongo/src/cancel.rs`
- Modify: `crates/driver-mongo/src/session.rs` (wait for the grace period, then fall back to a rebuild)
- Test: `crates/driver-mongo/tests/cancel.rs`

**Interfaces:**
- **`MongoCanceller`** holds `Arc<RwLock<Client>>` and `Arc<Mutex<Option<Running>>>`. It does `$currentOp {allUsers: false}`, matches on the comment, then runs `killOp`. With no op running, or no op matched, it does nothing (§5.5).
- **`execute_command` grace handling** (§5.5):
  - It emits `CancelSent`, then waits for `cancel_grace`.
  - When the grace expires, it emits `GraceExpired`, drops the operation future, swaps in a rebuilt `Client`, and calls `shutdown().immediate(true)` on the old one in a background task.
  - `late_completion` is set when the command finishes after stop. `outcome_unknown` follows the command's class.
- `close()` with a live cursor must not hang.

- [ ] **Step 1: Write the failing tests:**
  - `cancel_slow_find_kills_op`: `$where` with sleep. The result is `Cancelled { outcome_unknown: false }`, the next command works, and `reconnects() == 0`.
  - `cancel_write_is_outcome_unknown`
  - `cancel_between_commands_is_noop`
  - `tiny_grace_rebuilds_client`: `reconnects() == 1`, and `current_database()` is unchanged.
  - `close_with_open_cursor_returns`
  - `retries_are_off`: as root, set `configureFailPoint: "failCommand"` with `{failCommands: ["insert"], closeConnection: true}` for one hit. Run one `insertOne`: it returns `ConnectionLost { outcome_unknown: true }`, and with profiler level 2 on the test db, exactly one `insert` is recorded (no retry).
- [ ] **Step 2: Run** them. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p dnt-driver-mongo --test cancel` with `DNT_TEST_DB=1`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(mongo): cancel by killOp with client rebuild fallback`.

### Task 11: Introspector, page, and count

**Files:**
- Create: `crates/driver-mongo/src/introspect.rs`, and `crates/driver-mongo/src/data.rs` (the `page` and `count` half)
- Test: `crates/driver-mongo/tests/browse.rs`, plus unit tests in `data.rs`

**Interfaces:**
- `Introspector` follows §6.1 exactly: authorized lists, the fixed sample `[{$sort: {_id: -1}}, {$limit: 100}]`, `mixed(…)` types, indexes, `object_ddl` for collections and views.
- `DataAccess::page` and `count` follow §6.2 exactly: the operator table, `IsNotNull`, anchored `Like`, coercion and its error text, `_id` as the last sort key.
- The raw filter goes through `parse_document`, then the driver's own check for `$where`, `$function` and `$accumulator` at any depth, then the "must be a document" check.

- [ ] **Step 1: Write the failing tests:**
  - Unit tests:
    - `filter_operator_table`
    - `like_is_anchored`
    - `coercion_by_db_type`
    - `coercion_error_text`
    - `raw_filter_mongosh_and_ejson`
    - `raw_filter_blocks_server_js`
    - `sort_tiebreak_id`
  - Integration tests:
    - `lists_collections_and_views`
    - `table_detail_mixed_and_stable` (two calls give equal fingerprints)
    - `page_with_filters`
    - `count_matches_page`
- [ ] **Step 2: Run** them. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p dnt-driver-mongo` with `DNT_TEST_DB=1`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(mongo): introspector and data view reads`.

### Task 12: Edits and `execute_atomic`

**Files:**
- Modify: `crates/driver-mongo/src/data.rs` (`build_edits`), `crates/driver-mongo/src/session.rs` (`execute_atomic`)
- Test: `crates/driver-mongo/tests/edits.rs`, plus unit tests

**Interfaces:**
- `build_edits` follows §6.3 exactly:
  - the `CellInput` table and its refusals;
  - `_id` rebuilt through `value_to_bson`;
  - `text` in mongosh form, for which `parse(text)` reproduces the payload.
- `execute_atomic` follows §6.4 exactly:
  - `ClientSession`, `start_transaction`, commands in order, `abort` on error with `rolled_back: true`;
  - a single retry of commit on `UnknownTransactionCommitResult`, and a single retry of abort;
  - `DocumentNotFound` when `matchedCount == 0`.
- On standalone, `execute_atomic` keeps the default `Unsupported`, and the backend never calls it, because `atomic != Yes` there.

- [ ] **Step 1: Write the failing tests:**
  - Unit tests:
    - `cell_input_table`
    - `id_cannot_change`
    - `dotted_or_dollar_fields_refused`
    - `default_and_loaded_refused`
    - `edit_text_parses_back`
  - Integration tests:
    - `update_unset_insert_delete`
    - `binary_id_document_editable`
    - `nested_json_edit_keeps_types`
    - `atomic_apply_rolls_back` (`mongo80`)
    - `standalone_apply_not_atomic` (`mongo60`)
    - `deleted_elsewhere_is_document_not_found`
- [ ] **Step 2: Run** them. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p dnt-driver-mongo` with `DNT_TEST_DB=1`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(mongo): document edits and atomic apply`.

### Task 13: Register the driver, app end-to-end, full verification, and hand-off

**Files:**
- Modify: `src-tauri/Cargo.toml`, and `src-tauri/src/state.rs` (`default_drivers()` adds `MongoDriver`)
- Test: `src-tauri/tests/mongo_driver.rs` (new)
- Create: `docs/superpowers/plans/2026-11-05-mongo-driver-followups.md`

- [ ] **Step 1: Write the failing tests:**
  - `available_engines_include_mongo`
  - `language_for_mongo_splits_and_redacts`
  - `mongo_end_to_end` (`require_db!`, `mongo80`, through `AppState`):
    - open the connection and list databases (system dbs hidden);
    - plan and run `db.orders.find({})`;
    - open the data view and edit one cell, which goes through `execute_atomic`;
    - a History row for `db.runCommand({createUser: "u", pwd: "p", roles: []})` contains `***`.
  - `guard_names_admin_for_admin_drop`: a Production plan for `db.adminCommand({dropDatabase: 1})` has `confirm_name == "admin"`.
  - `update_many_empty_filter_needs_name_on_production`
  - `mongo_through_ssh_tunnel`: through the compose `ssh` service to `mongo60`, then `ping`.
- [ ] **Step 2: Run** them. Expected: FAIL.
- [ ] **Step 3: Implement** the registration.
- [ ] **Step 4: Run the full CI set:**
  - `cargo fmt --all --check`
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - `cargo test --workspace`, both without and with `DNT_TEST_DB=1 --test-threads=1`
  - `pnpm --dir ui lint`, `pnpm --dir ui test`
  - `pnpm --dir e2e-web typecheck`
  - `$env:E2E_WEB_PORT='5176'; pnpm --dir e2e-web test`
  - `cargo tree -i aws-lc-rs`, which must report not found

  Expected: PASS.
- [ ] **Step 5: Write the hand-off doc.** Cover decisions carried forward, review findings, the deferred scope (`bulkWrite`, x509, Verify CA, the explain panel), and the Redis plan's needed adjustments (`execute_atomic` exists; `ValueRange.cursor` is still pending in the Redis plan). Commit `feat(app): register the MongoDB driver` and `docs: MongoDB driver hand-off`.
