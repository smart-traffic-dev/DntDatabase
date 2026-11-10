# MongoDB Tooling Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give MongoDB connections seven day-to-day tools: `bulkWrite`, keepalive for every engine, schema analysis,
a whole-document JSON editor, index management, an Explain panel, and export/import. Every write keeps the existing
guards.

**Architecture:**
- **I/O placement.** New Mongo work that needs I/O is a method on `CommandSession` (default `Unsupported`), implemented
  in `crates/driver-mongo` and reached from `src-tauri` through a pooled data session. New writes are built as
  `ParsedCommand`s (`DataAccess`) and go through the existing edit-plan store, verdict and `data_apply`.
- **UI.** Per-kind decisions are exhaustive switches in `ui/src/engine/index.ts`.

**Tech Stack:**
- Rust: tokio, the `mongodb` crate 3.x (already a dependency), serde, specta / tauri-specta.
- UI: React, TS, zustand, Monaco, vitest.
- e2e: Playwright with the fake IPC backend.

**Spec:** `docs/superpowers/specs/2026-11-09-mongo-tooling-design.md`

## Global Constraints

- **Workspace.**
  - Work in `D:\DntDatabase\.claude\worktrees\mongo-tools` (branch `mongo-tools`). Never touch `D:\DntDatabase` itself.
  - The shell is PowerShell 5.1 (no `&&`) or Git Bash.
- **Unchanged behaviour.** MySQL, PostgreSQL and Redis behaviour stays the same, existing tests pass, and SQL
  snapshots stay identical.
- **Safety promises.**
  - Every write goes through a guarded plan.
  - Read-only connections never write.
  - Production asks for confirmation.
  - Nothing is retried automatically.
  - Secrets never reach History or logs.
- **UI copy** is English and uses the exact strings in this plan and the spec.
- **No kind literals** (`'mysql'|'postgres'|'mongo'|'redis'`) outside `ui/src/engine/` (ESLint `KIND_COMPARISON`).
- **Rust kind branching** uses exhaustive `match` only (the `kind_branches` test).
- **Dependencies.** No new crates or npm packages.
- **Bindings.**
  - Every Tauri signature change needs `cargo run -p dntdatabase --example export_bindings`, after which
    `bindings_are_current` (`src-tauri/tests/commands.rs:329`) passes.
  - Register new commands in `specta_builder()` (`src-tauri/src/commands/mod.rs:146-302`).
- **Encoding.** Files are UTF-8 without BOM (the `no_bom` test). Never use PowerShell `Set-Content -Encoding utf8`.
- **TDD.** Write the failing test first, show RED, then GREEN.
- **Verification per task:**
  - `cargo fmt --all`
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - the touched crates' tests (DB tests: `$env:DNT_TEST_DB='1'`, `--test-threads=1`, with the dnt-mongo-wt
    containers mongo60:27060 and mongo80:27080)
  - `pnpm --dir ui lint`, `pnpm --dir ui test`
  - the touched e2e spec: `$env:E2E_WEB_PORT='5176'; pnpm --dir e2e-web test <spec>`
- **Commits.** One commit per task, ending with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. Never
  push, amend or skip hooks.
- **Floor.** MongoDB 6.0. Test on 6.0 and 8.0.

## Deviations from the spec (found while mapping the code)

- **C6 setting location.** Settings has no Connections section, so `Keep connections alive` goes in **Advanced**.
- **C6 status event.** No connection-state event exists today. Task 2 adds `connection://state`, emitted from
  `set_state`, so every state change reaches the UI and not only those caused by the keepalive.
- **C6 custom state.** Query-tab sessions have no "custom state" flag. The pinging rule is: idle and not in a
  transaction.
- **C1 type counts.** `FieldStat.types` uses `TypeCount { name, count }`, not a tuple, so the TS binding has names.
- **C2 document text.** `RowEdit::Replace` / `InsertDocument` carry `doc: String`, which is console literal text,
  parsed in the driver. `document_fetch` returns `String` and errors when the document is gone.
- **C4 whole-collection export.** It streams the cursor through `data_exec` instead of paging by 1,000. SQL
  whole-table export already streams the same way.
- **C4 JSON lines.** `Jsonl` is added as an export format for every engine.

## Review Focus

1. **`_id` equality across literal forms.** A replacement whose `_id` is written as `ObjectId("…")`, `{"$oid": "…"}`
   or a plain value of the same BSON type must count as equal to the row's `_id`. The comparison is on parsed `Bson`,
   never on text. (Task 6)
2. **Huge or deep arrays in the schema sample.** A document holding a 100k-element array, or nesting 50 levels deep,
   must not hang or blow the path cap. The walker visits at most the first 1,000 elements of an array and stops at
   depth 4. (Task 4)
3. **Keepalive never touches a busy session.** A ping never runs on a session that is borrowed, running a command,
   applying an edit, or inside a transaction. Pooled sessions are taken out of the pool while they are pinged.
   (Task 2)
4. **Missing fields in exports.** A document that lacks a field another row has must not gain `"field": null` in JSON
   or JSON-lines export. The field is omitted. CSV writes an empty cell. (Task 12)
5. **An import row that defeats inference.** A CSV column inferred `int` from its sample that later holds `"n/a"`
   counts as a per-record error in the job log. It neither aborts the batch nor silently becomes 0. (Task 13)

---

### Task 1: `bulkWrite` in the console (C7)

**Files:**
- Modify: `crates/driver-mongo/src/shell/parse.rs` (method table `:115-141`, `coll_sig` `:379`, `check` `:466`)
- Modify: `crates/driver-mongo/src/shell/classify.rs` (`coll_effect` `:305`, `any_unbounded` `:203`)
- Modify: `crates/driver-mongo/src/exec.rs` (`coll_method` `:605`, `reply_output` `:1163`, `allowed_read_only` `:80`)
- Modify: `crates/driver-mongo/src/data.rs` (`coll_text` `:464`), only if the round-trip needs it
- Test: unit tests in those modules; `crates/driver-mongo/tests/console.rs`

**Interfaces:**
- Produces: `CollMethod::BulkWrite = "bulkWrite"`. Nothing else later depends on it.

- [ ] **Step 1: Write failing parse and classify tests** (`parse.rs` / `classify.rs` test modules)
  - `bulk_write_parses_every_op`: `db.c.bulkWrite([{insertOne: {document: {a: 1}}}, {updateOne: {filter: {a: 1},
    update: {$set: {b: 2}}, upsert: true}}, {updateMany: {filter: {a: 1}, update: {$set: {c: 1}}}}, {replaceOne:
    {filter: {a: 1}, replacement: {a: 2}}}, {deleteOne: {filter: {a: 2}}}, {deleteMany: {filter: {x: 1}}}], {ordered:
    false})` parses with method `BulkWrite`.
  - `bulk_write_unknown_op_is_refused`: `db.c.bulkWrite([{upsertOne: {}}])` gives an error whose message is
    `Unknown bulkWrite operation upsertOne`.
  - `bulk_write_needs_ops`: `db.c.bulkWrite([])` gives `bulkWrite needs at least one operation`.
  - `bulk_write_is_write`: class is `Write` and targets are `[c]`.
  - `bulk_write_unbounded`: true when a `deleteMany` has filter `{}` or an `updateMany` has no filter; false when every
    `*Many` filter is selective; true when an op is not a document (fail-closed via `any_unbounded`).
  - `bulk_write_round_trips`: `coll_text` of the parsed command re-parses to the same args.
  - `bulk_write_refused_read_only`: `allowed_read_only` is false.
  - `bulk_write_redacts`: a string literal inside a `bulkWrite` filter is redacted like other commands. Copy the shape
    of an existing redaction test.
- [ ] **Step 2: Run them.**
  - Command: `cargo test -p dnt-driver-mongo --lib bulk_write`
  - Expected: FAIL (unknown method `bulkWrite`).
- [ ] **Step 3: Implement**
  - **Parse and classify.** Add `BulkWrite` to the method table: arity 1–2, arg 1 an array of single-key documents,
    arg 2 an options document with `ordered: bool`. Validate op names in `check`. Classify as `Write` with
    `unbounded = any_unbounded(ops, open)`, where `open` looks at `updateMany.filter` / `deleteMany.filter` only.
  - **Execute.** In `coll_method`, group consecutive ops into runs by server command:
    - `insertOne` → `insert`;
    - `updateOne`, `updateMany` (`multi: true`) and `replaceOne` → `update`;
    - `deleteOne` (`limit: 1`) and `deleteMany` (`limit: 0`) → `delete`.

    Send the runs in order, each with the user's `ordered`.
    - `ordered: true` stops after the first run with `writeErrors`.
    - `ordered: false` sends every run.
  - **Output.** One `result` row: `insertedCount`, `matchedCount`, `modifiedCount`, `deletedCount`, `upsertedCount`,
    and `upsertedIds` as a document keyed by the global op index. `affected` = inserted + matched + deleted.
  - **Errors.** If any write error occurred, return `DbError` (outcome known) with the message
    `bulkWrite: <k> operation(s) failed; first at index <i>: <server message>. Applied: inserted <a>, matched <b>,
    modified <c>, deleted <d>, upserted <e>.`
- [ ] **Step 4: Write failing integration tests** (`tests/console.rs`, both targets)
  - `bulk_write_ordered_stops_at_duplicate`: insert `_id` 1, 1, 2 with ordered (default). The error names index 1, the
    collection holds only `_id` 1, and the message has `inserted 1`.
  - `bulk_write_unordered_continues`: the same ops with `{ordered: false}`. The error names index 1, and the
    collection holds `_id` 1 and 2.
  - `bulk_write_mixed_counts`: insert, update and delete in one call. The result row's counts and `affected` match.
- [ ] **Step 5: Run them.**
  - Command: `$env:DNT_TEST_DB='1'; cargo test -p dnt-driver-mongo --test console bulk_write -- --test-threads=1`
  - Expected: PASS on mongo60 and mongo80.
- [ ] **Step 6: Commit.** Message: `feat(mongo): bulkWrite in the console`.

### Task 2: Keepalive backend (C6)

**Files:**
- Create: `src-tauri/src/keepalive.rs`, holding the ping loop.
- Modify: `src-tauri/src/session_manager.rs`:
  - `ConnectionRuntime` `:1177`;
  - `open` `:583`, where the loop is spawned next to `spawn_schema_load` `:658` and pushed into `schema_tasks`;
  - `set_state` `:1258`;
  - `reopen` `:1328`.
- Modify: `src-tauri/src/data_sessions.rs`:
  - add `since: Instant` to `Idle` `:61`;
  - `give_back` `:877`;
  - a new `take_stale(&self, older_than: Duration) -> Vec<(Option<String>, Idle)>`.
- Modify: `src-tauri/src/query_sessions.rs`:
  - add `last_used: Instant` to `TabSession` `:259`, set at the end of every run;
  - a new `ping_idle_tabs(&self, older_than: Duration, timeout: Duration)`.
- Modify: `src-tauri/src/commands/settings.rs` and `commands/mod.rs`, for the setting commands and the registration.
- Modify: `src-tauri/src/main.rs:57`, which installs the event emitter (the `JobEvents` pattern).
- Test: unit tests in `keepalive.rs`; `src-tauri/tests/keepalive.rs` (new; DB tests on Mongo and PG17).

**Interfaces:**
- Produces:
  - `saved_keepalive_seconds(repo) -> u32` (key `keepalive_seconds`, default `60`);
  - commands `get_keepalive_seconds() -> u32` and `set_keepalive_seconds(seconds: u32)`;
  - event name `CONN_STATE_EVENT = "connection://state"`;
  - payload `ConnStateEvent { id: Uuid, state: ConnState }` (specta type, exported in bindings);
  - `trait ConnEvents: Send + Sync { fn state_changed(&self, id: Uuid, state: &ConnState); }`, installed on
    `SessionManager`.

- [ ] **Step 1: Write failing unit tests** (`keepalive.rs`, fake `Session` with a scripted `ping`)
  - `zero_interval_pings_nothing`
  - `pool_session_idle_longer_than_interval_is_pinged_and_returned`
  - `failed_pool_ping_drops_the_session`: the pool is empty afterwards.
  - `fresh_pool_session_is_not_pinged`
  - `tab_session_in_transaction_is_never_pinged`
  - `locked_tab_slot_is_skipped`: the slot's mutex is held, as if a run were in progress. The tick uses `try_lock` and
    skips it.
  - `failed_tab_ping_marks_reopen`: `reopen == true`, and the session is dropped.
  - `ping_times_out_after_5s`: a ping that never answers counts as failed after 5 s. Use paused tokio time.
  - `one_tick_in_flight`: a second tick while the first is running does nothing.
  - `metadata_ping_failure_sets_reconnecting_then_connected`: the events received are `Reconnecting`, then
    `Connected`.
- [ ] **Step 2: Run them.**
  - Command: `cargo test -p dntdatabase --lib keepalive`
  - Expected: FAIL (module missing).
- [ ] **Step 3: Implement `pub(crate) async fn keepalive_loop(rt: Weak<ConnectionRuntime>, interval: Arc<AtomicU32>)`**
  - **Interval.** It reads the interval each tick. While 0, it sleeps 5 s and checks again.
  - **Each tick:**
    1. Metadata: `with_session(None, |s| s.ping())`, under a 5 s timeout. A `ConnectionLost` goes through the existing
       `reopen`.
    2. The pool: `data.take_stale(interval)`. Ping each session; `give_back` the ones that answer and drop the rest.
    3. Tabs: `queries.ping_idle_tabs(interval, 5s)`.
  - **Events.** `set_state` calls `ConnEvents::state_changed`, so every state change, and not only the keepalive's, is
    emitted.
  - **Lifetime.** The loop holds only a `Weak` and exits when the upgrade fails or `closed` is set.
  - **Setting.** `AppState` holds `keepalive: Arc<AtomicU32>`, loaded at startup and updated by
    `set_keepalive_seconds`.
- [ ] **Step 4: Run the unit tests.**
  - Command: `cargo test -p dntdatabase --lib keepalive`
  - Expected: PASS.
- [ ] **Step 5: Write and run a failing DB test** (`src-tauri/tests/keepalive.rs`)
  - Test: `keepalive_detects_a_killed_connection_and_reopens`. Run it on Mongo80 and PG17.
    1. Open with a 1 s interval.
    2. Kill the metadata connection server-side: PG `pg_terminate_backend`; Mongo `killAllSessions` or failCommand
       `closeConnection` on `ping`.
    3. Within 10 s the events include `Reconnecting` then `Connected`, and a following `table_detail` succeeds.
  - Command: `$env:DNT_TEST_DB='1'; cargo test -p dntdatabase --test keepalive -- --test-threads=1`
  - Expected: PASS.
- [ ] **Step 6: Regenerate bindings and run `bindings_are_current`.** Commit with the message
  `feat: keep connections alive with a background ping`.

### Task 3: Keepalive UI (C6)

**Files:**
- Modify:
  - `ui/src/store/appSettings.ts` (`useAppSettings` `:71`): `keepaliveSeconds`, `DEFAULT_KEEPALIVE_SECONDS = 60`, the
    loader and the setter.
  - `ui/src/dialogs/SettingsDialog.tsx` (sections `:616-621`): the Advanced section gains a `Keep connections alive`
    number input, in seconds, with the hint `0 turns it off`.
  - `ui/src/store/connections.ts`, or wherever `useConnections` lives: subscribe to `connection://state` and keep a
    `states: Record<string, ConnState>` map.
  - `ui/src/components/StatusBar.tsx:186-230`: for the active connection, `Reconnecting…` while reconnecting; when the
    state is failed, `Connection lost` plus a `Reconnect` button that calls the existing reopen in
    `ui/src/store/reopen.ts:63`.
  - `ui/src/components/ConnectionTree.tsx:248-272`: a failed connection row gets the class `conn-lost` and the title
    `Connection lost`.
- Modify: `e2e-web/src/backend.ts`. Fake `get_keepalive_seconds` and `set_keepalive_seconds`, plus a test hook that
  emits `connection://state`.
- Test: `StatusBar.test.tsx`, `SettingsDialog.test.tsx`, `ConnectionTree.test.tsx`;
  `e2e-web/specs/connection-state.spec.ts` (new).

**Interfaces:**
- Consumes: Task 2's commands, event name and `ConnStateEvent`.

- [ ] **Step 1: Write failing tests.**
  - Status bar:
    - `shows Reconnecting… on a reconnecting event`;
    - `shows Connection lost and Reconnect on failed; Reconnect calls connection_reopen`, using the command name the
      reopen store already calls;
    - `clears on connected`.
  - Settings: `saves keep-alive seconds; 0 allowed; negative refused`.
  - Tree: `failed connection row has conn-lost`.
- [ ] **Step 2: Run them.**
  - Command: `pnpm --dir ui test -- StatusBar SettingsDialog ConnectionTree`
  - Expected: FAIL.
- [ ] **Step 3: Implement** the items in the Files list.
- [ ] **Step 4: Run the unit tests and the e2e spec.**
  - Commands: `pnpm --dir ui test`, and `pnpm --dir e2e-web test connection-state`.
  - The e2e spec covers two cases: the fake emits failed, so the bar shows `Connection lost`; Reconnect, then the bar
    is normal again.
  - Expected: PASS.
- [ ] **Step 5: Commit.** Message: `feat(ui): connection state in the status bar; keep-alive setting`.

### Task 4: Schema analysis backend (C1)

**Files:**
- Modify: `crates/core/src/meta.rs`. Add:
  - `CollectionSchema { sampled: u32, paths: Vec<FieldStat>, truncated: bool }`;
  - `FieldStat { path: String, depth: u8, present: u32, types: Vec<TypeCount> }`;
  - `TypeCount { name: String, count: u32 }`.

  All three derive `Serialize, Deserialize, specta::Type, Clone, Debug, PartialEq`.
- Modify: `crates/core/src/command.rs`. Add to `CommandSession`:
  `async fn collection_schema(&self, obj: &ObjectRef, sample: u32, opts: &ExecOptions) -> Result<CollectionSchema,
  DbError>`, with a default `Unsupported`.
- Create: `crates/driver-mongo/src/schema.rs`, holding `pub(crate) fn walk(docs: &[Document]) -> CollectionSchema`.
- Modify: `crates/driver-mongo/src/session.rs`, which implements it. It runs `aggregate [{$sample: {size}}]` through
  the Read path (`run_watched`, honouring stop and `maxTimeMS`).
- Modify: `src-tauri/src/commands/data.rs`. Add the command
  `collection_schema(state, id: Uuid, obj: ObjectRef, sample_size: u32, request_key: String) ->
  Result<CollectionSchema, AppError>`.
  - It refuses with `Unsupported` unless the family is Command and the kind is Mongo (exhaustive match on `DbKind`).
  - It borrows a data session under `request_key`, so `data_cancel` stops it.
  - A `sample_size` other than 100, 1000 or 5000 gives `Sample size must be 100, 1000 or 5000`.
- Test: `schema.rs` unit tests; `crates/driver-mongo/tests/browse.rs`.

**Interfaces:**
- Produces:
  - the `collection_schema` command;
  - `CollectionSchema`, `FieldStat` and `TypeCount` in `ui/src/bindings.ts`;
  - `FieldStat.path` syntax `a.b` / `items[].sku`, used by Tasks 5, 9 and 14.

- [ ] **Step 1: Write failing walker tests** (`schema.rs`)
  - `nested_paths_use_dots`: `{a: {b: 1}}` gives the paths `a` (depth 1, object) and `a.b` (depth 2, int).
  - `array_elements_use_brackets`: `{items: [{sku: "x"}, {sku: 2}]}` gives the paths `items` (array), `items[]`
    (object) and `items[].sku` with `types = [{int,1},{string,1}]` (sorted by count, then name).
  - `present_counts_null`: `{a: null}` gives `present = 1` with type `null`.
  - `present_once_per_document`: one document with `items: [{sku:1},{sku:2}]` gives `items[].sku` `present = 1`,
    `int` count 1.
  - `depth_stops_at_4`: the nesting `a.b.c.d.e` emits nothing deeper than depth 4.
  - `truncates_at_2000_paths`: a document with 2,500 distinct keys gives `paths.len() == 2000` and `truncated == true`.
  - `huge_array_is_bounded` (Review Focus 2): a 100,000-element array is walked over its first 1,000 elements and
    finishes in under 1 s.
  - `id_first_then_first_seen_order`: `_id` is the first path, and parents come before their children.
  - `type_names_follow_bson_type_name`: use `rows.rs:322` `bson_type_name`.
- [ ] **Step 2: Run them.**
  - Command: `cargo test -p dnt-driver-mongo --lib schema`
  - Expected: FAIL.
- [ ] **Step 3: Implement `walk`.**
  - Count types once per document per path.
  - Cap arrays at their first 1,000 elements.
  - Cap depth at 4.
  - Keep insertion order with a map from path to index.
- [ ] **Step 4: Write and run a failing DB test.**
  - Test: `collection_schema_on_fixture` (`browse.rs`, both targets). The schema of `dnt_fixture.orders` with sample
    1000 has `sampled` equal to the fixture's document count, includes `_id` with type `objectId`, and the read-only
    connection succeeds.
  - Command: `$env:DNT_TEST_DB='1'; cargo test -p dnt-driver-mongo --test browse collection_schema -- --test-threads=1`
  - Expected: PASS.
- [ ] **Step 5: Wire the Tauri command and its registration.**
  - Write a test in `src-tauri/tests/mongo_driver.rs`: `collection_schema_command_refuses_bad_size`.
  - Regenerate the bindings.
  - Commit with the message `feat(mongo): schema analysis of a collection sample`.

### Task 5: Schema sub-tab UI (C1)

**Files:**
- Modify: `ui/src/tabs/objectSections.tsx:73` (`subTabsOf`). Collection gives `['data','schema','ddl']`. Task 9 adds
  `indexes`.
- Create: `ui/src/tabs/schema/SchemaView.tsx`, `ui/src/tabs/schema/schemaModel.ts` (the tree, the filter and the
  percentages).
- Modify: `e2e-web/src/commandData.ts`, which fakes `collection_schema` from the fixture docs (`commandFixtures.ts`).
- Test: `ui/src/tabs/schema/schemaModel.test.ts`, `SchemaView.test.tsx`; `e2e-web/specs/mongo-schema.spec.ts`.

**Interfaces:**
- Consumes: `collection_schema(id, obj, sample_size, request_key)` and `FieldStat`.
- Produces: `schemaModel.ts` `export function fieldPaths(schema: CollectionSchema): string[]`, used by Tasks 9 and 14.

- [ ] **Step 1: Write failing tests.**
  - `schemaModel`:
    - `percent of sampled`;
    - `mixed when more than one type`;
    - `filter keeps ancestors of matches`;
    - `collapse hides descendants`.
  - `SchemaView`:
    - the header shows `Sample of 1000 documents`;
    - changing the select to 5000 refetches with `sample_size: 5000`;
    - `Refresh` refetches;
    - `sampled: 0` shows `This collection has no documents.`;
    - the row menu `Copy path` copies `items[].sku`.
- [ ] **Step 2: Run them.**
  - Command: `pnpm --dir ui test -- schema`
  - Expected: FAIL.
- [ ] **Step 3: Implement.** Columns: the path indented by depth with a collapse chevron, presence as a percentage
  plus a thin bar, and type chips (`string 80%`) plus a `mixed` badge. Reuse the filter input style of
  `ObjectInfoPanel.tsx:358`.
- [ ] **Step 4: Run the unit tests and the e2e spec.**
  - Commands: `pnpm --dir ui test`, and `pnpm --dir e2e-web test mongo-schema`. The spec opens the collection, clicks
    `Schema` and sees the fields.
  - Expected: PASS.
- [ ] **Step 5: Commit.** Message: `feat(ui): schema sub-tab for collections`.

### Task 6: Whole-document edits backend (C2)

**Files:**
- Modify: `crates/core/src/edit.rs:53`. `RowEdit` gains:
  - `Replace { row: RowRef, doc: String }`
  - `InsertDocument { doc: String }`

  `doc` is console literal text (Extended JSON v2 plus the shell helpers).
- Modify: `crates/query/src/edit.rs` (`:624-659`). SQL refuses:
  - `Replace` with `Replacing a whole row is not available for this engine`;
  - `InsertDocument` with `Adding a whole document is not available for this engine`.
- Modify: `crates/driver-mongo/src/data.rs` (`build_edits` `:547`). Parse `doc` with `literal.rs:43`
  `parse_document`.
  - **Replace** → `replaceOne({_id: <row _id>}, doc)`.
    - If the doc has no `_id`, insert the row's `_id` first.
    - If it has one, it must equal the row's `_id` as `Bson`, else `_id cannot be changed`.
    - Any top-level key starting with `$` gives `A replacement document cannot contain update operators`.
    - `Payload.document` is set, so `check_found` gives `DocumentNotFound` on matched 0.
  - **InsertDocument** → `insertOne(doc)`.
  - The text comes from `coll_text`.
- Modify: `crates/core/src/command.rs`. Add to `CommandSession`:
  `async fn fetch_document(&self, obj: &ObjectRef, row: &RowRef, opts: &ExecOptions) -> Result<String, DbError>`,
  defaulting to `Unsupported`.
  - Mongo runs a Read `find({_id}).limit(1)` and returns canonical Extended JSON, pretty-printed with 2 spaces.
  - When nothing is found the error is `Document <_id as relaxed EJSON> no longer exists`.
- Modify: `src-tauri/src/commands/data.rs`. Add the command
  `document_fetch(state, id: Uuid, obj: ObjectRef, row: RowRef) -> Result<String, AppError>`, using the same
  data-session borrowing as Task 4.
- Test: unit tests in `data.rs` and `crates/query/src/edit.rs`; `crates/driver-mongo/tests/edits.rs`.

**Interfaces:**
- Produces: the `RowEdit::Replace` and `RowEdit::InsertDocument` serde forms (`{op: "replace", row, doc}`,
  `{op: "insert_document", doc}`; check `RowEdit`'s serde `rename_all`) and the `document_fetch` command.

- [ ] **Step 1: Write failing unit tests.**
  - `replace_builds_replace_one`
  - `replace_without_id_keeps_row_id`
  - `replace_with_changed_id_refused`
  - `replace_id_equal_across_forms` (Review Focus 1): the row `_id` is an ObjectId, and the doc `_id` is written both as
    `ObjectId("…")` and as `{"$oid":"…"}`. Both are accepted.
  - `replace_with_operator_refused`
  - `replace_not_found_is_document_not_found`, through the existing `check_found` test pattern
  - `insert_document_keeps_nesting`: `{a: {b: [1, {c: 2}]}}` arrives intact in the `insertOne` arg.
  - `invalid_doc_reports_parse_error`
  - `sql_refuses_replace_and_insert_document`
- [ ] **Step 2: Run them.**
  - Commands: `cargo test -p dnt-driver-mongo --lib replace insert_document`, and
    `cargo test -p dnt-query --lib whole`. Use the crate names in `Cargo.toml`.
  - Expected: FAIL.
- [ ] **Step 3: Implement** the items in the Files list.
- [ ] **Step 4: Write and run failing DB tests** (`tests/edits.rs`, both targets).
  - `replace_applies`
  - `replace_and_update_atomic_on_8`, on mongo80 only, through `execute_atomic`
  - `insert_document_applies`
  - `fetch_document_returns_full_doc_over_64k`: a 100 KiB string field comes back whole.
  - `fetch_missing_document_errors`
- [ ] **Step 5: Wire the command and regenerate the bindings.** Commit with the message
  `feat(mongo): replace or add a whole document`.

### Task 7: Document editor dialog (C2)

**Files:**
- Create: `ui/src/tabs/data/DocumentDialog.tsx`. It contains:
  - a `@monaco-editor/react` `Editor` with `language={MONGO_LANGUAGE}`, `useDntMonacoTheme` and `editorFontOptions`;
  - `Format` (`formatMongo`);
  - a validity line;
  - `Cancel` and `Stage` buttons.
- Create: `ui/src/query/mongo/validateDocument.ts`:
  `export function validateDocument(text: string): {ok: true} | {ok: false; message: string; line: number; column:
  number}`.
  - It uses `lexer.ts` and the formatter's parse.
  - It accepts exactly one object literal.
- Modify: `ui/src/store/dataEdits.ts:42` (`StageEdit`). Add:
  - `{kind: 'replace'; row: RowRef; doc: string}`
  - `{kind: 'insertDocument'; doc: string}`

  Map them to `RowEdit` in the apply path.
- Modify: `ui/src/tabs/DataView.tsx`:
  - the row menu `:915-949` gains `Edit document…`;
  - `Enter` on a row-number cell opens it;
  - the toolbar near `:772` gains `Add document…`.

  All are shown only when `hasDocumentEditor(kind)`.
- Modify: `ui/src/engine/index.ts`. Add `export function hasDocumentEditor(kind)`: an exhaustive switch, true only for
  mongo.
- Modify: `e2e-web/src/commandData.ts`. Fake `document_fetch`, and accept `replace` / `insert_document` in
  `data_preview` / `data_apply`.
- Test: `DocumentDialog.test.tsx`, `validateDocument.test.ts`, `dataEdits.test.ts`; `e2e-web/specs/mongo-document.spec.ts`.

**Interfaces:**
- Consumes: `document_fetch` and the `RowEdit` forms from Task 6.
- Produces: `hasDocumentEditor(kind)`.

- [ ] **Step 1: Write failing tests.**
  - `validateDocument`:
    - `{a: 1}` is ok;
    - `[1]` gives `A document must be an object`;
    - an unclosed brace reports line and column.
  - `DocumentDialog`:
    - the validity line shows `Valid document`;
    - `Stage` is disabled while invalid;
    - `Format` rewrites the text;
    - `Stage` calls `onStage(text)`.
  - `DataView`:
    - `Edit document…` calls `document_fetch`, then opens the dialog with its text;
    - staging marks the row modified;
    - `Add document…` stages an `insertDocument`;
    - the menu items are absent on a SQL tab;
    - the menu items are disabled when read-only, under the existing edit rule.
- [ ] **Step 2: Run them.**
  - Command: `pnpm --dir ui test -- DocumentDialog validateDocument dataEdits DataView`
  - Expected: FAIL.
- [ ] **Step 3: Implement** the items in the Files list.
- [ ] **Step 4: Run the unit tests and the e2e spec.**
  - Commands: `pnpm --dir ui test`, and `pnpm --dir e2e-web test mongo-document`. The spec does edit → Stage →
    preview shows `replaceOne` → Apply.
  - Expected: PASS.
- [ ] **Step 5: Commit.** Message: `feat(ui): edit and add whole documents as JSON`.

### Task 8: Index edits backend (C3)

**Files:**
- Modify: `crates/core/src/edit.rs`. Add:
  - `IndexEdit { Create { keys: Vec<IndexKeyPart>, options: IndexOptions }, Drop { name: String } }`, serde tag `op`;
  - `IndexKeyPart { field: String, key: IndexKey }`;
  - `IndexKey { Asc, Desc, Text, Hashed, Geo2dsphere }`;
  - `IndexOptions { name: Option<String>, unique: bool, sparse: bool, hidden: bool, expire_after_seconds:
    Option<i64>, partial_filter: Option<String> }`. `partial_filter` is console literal text.
- Modify: `crates/core/src/data_access.rs:14`. Add
  `fn build_index_edit(&self, obj: &ObjectRef, edit: &IndexEdit) -> Result<Vec<ParsedCommand>, DbError>`, defaulting
  to `Err(Unsupported)`.
- Modify: `crates/driver-mongo/src/data.rs`.
  - `Create` → `createIndex(keys, options)`, with key values `1` / `-1` / `"text"` / `"hashed"` / `"2dsphere"`. Leave
    out options that are false or None. Map `expire_after_seconds` to `expireAfterSeconds` and `partial_filter` to
    `partialFilterExpression`, parsed with `parse_document`.
  - `Drop` → `dropIndex("<name>")`.
  - Dropping `_id_` gives `The _id index cannot be dropped`.
  - No keys gives `An index needs at least one key`.
  - `expire_after_seconds` with more than one key gives `A TTL index has exactly one key`.
  - Both are classified through the existing classifier, so they come out as class `Ddl` with targets `[collection]`.
- Modify: `src-tauri/src/edit_plans.rs`. Add
  `pub async fn index_edit_preview(&self, id: Uuid, obj: ObjectRef, edit: &IndexEdit) -> Result<EditPreview, DbError>`,
  modelled on `key_edit_preview` (`:1694`): it stores a `StoredPlan::Command` with `transaction: false`.
- Modify: `src-tauri/src/commands/data.rs`. Add the command
  `index_edit_preview(state, id: Uuid, obj: ObjectRef, edit: IndexEdit) -> Result<EditPreview, AppError>`. It is
  applied with the existing `data_apply`.
- Test: unit tests in `data.rs`; `crates/driver-mongo/tests/edits.rs`; `src-tauri/tests/mongo_driver.rs`.

**Interfaces:**
- Produces: `IndexEdit`, `IndexKeyPart`, `IndexKey` and `IndexOptions` in the bindings, and the `index_edit_preview`
  command.

- [ ] **Step 1: Write failing unit tests.**
  - `create_index_text`: text `db.orders.createIndex({status: 1, total: -1}, {name: "s_t", unique: true})`.
  - `create_ttl_and_partial`
  - `drop_id_index_refused`
  - `ttl_needs_one_key`
  - `no_keys_refused`
  - `index_edit_is_ddl_targeting_collection`
- [ ] **Step 2: Run them.**
  - Command: `cargo test -p dnt-driver-mongo --lib index_edit`
  - Expected: FAIL.
- [ ] **Step 3: Implement** the items in the Files list.
- [ ] **Step 4: Write and run failing tests.**
  - `index_create_and_drop_round_trip` (DB, both targets): preview → apply creates the index, `table_detail` lists it,
    then drop removes it.
  - `index_edit_on_read_only_is_blocked`
  - `index_edit_production_needs_confirmation`
- [ ] **Step 5: Wire the command and regenerate the bindings.** Commit with the message
  `feat(mongo): create and drop indexes through a preview`.

### Task 9: Indexes sub-tab UI (C3)

**Files:**
- Modify: `ui/src/tabs/objectSections.tsx:73`. Collection gives `['data','schema','ddl','indexes']`.
- Create:
  - `ui/src/tabs/indexes/IndexesView.tsx`, which lists `table_detail` `IndexDetail` rows;
  - `ui/src/tabs/indexes/CreateIndexDialog.tsx`, holding the key rows (a field combobox fed by `fieldPaths(schema)`,
    falling back to `table_detail` columns, plus a direction/type select), name, unique, sparse, hidden, `TTL
    (seconds)`, and `Partial filter` (Monaco `mongo`, validated with `validateDocument`).
- Reuse: `EditPreviewDialog` (`ui/src/tabs/data/EditPreviewDialog.tsx:58`) with `family='command'`, plus `data_apply`.
  After a successful apply, reload `table_detail` and invalidate the Info panel for the object.
- Modify: `e2e-web/src/commandData.ts`. Fake `index_edit_preview` and the apply.
- Test: `IndexesView.test.tsx`, `CreateIndexDialog.test.tsx`; `e2e-web/specs/mongo-indexes.spec.ts`.

**Interfaces:**
- Consumes: `index_edit_preview` (Task 8), `fieldPaths` (Task 5) and `table_detail`.

- [ ] **Step 1: Write failing tests.**
  - The columns are name, keys (`status ↑, total ↓`, `text`), unique, sparse, TTL, partial, hidden and size.
  - `Drop` on `_id_` is not offered.
  - On a read-only connection the list shows no `Create index…` and no `Drop`.
  - The create dialog builds `IndexEdit.Create` with the selected keys and options.
  - TTL with two keys disables `Preview` and shows `A TTL index has exactly one key`.
  - An apply refreshes the list.
- [ ] **Step 2: Run them.**
  - Command: `pnpm --dir ui test -- IndexesView CreateIndexDialog`
  - Expected: FAIL.
- [ ] **Step 3: Implement** the items in the Files list.
- [ ] **Step 4: Run the unit tests and the e2e spec.**
  - Commands: `pnpm --dir ui test`, and `pnpm --dir e2e-web test mongo-indexes`. The spec creates an index, sees
    `createIndex` in the preview, applies, sees the new row, then drops it.
  - Expected: PASS.
- [ ] **Step 5: Commit.** Message: `feat(ui): indexes sub-tab for collections`.

### Task 10: Explain backend for Mongo (C5)

**Files:**
- Modify: `crates/query/src/explain.rs`.
  - Add `ExplainFormat::MongoJson` (`:14`).
  - Add `examined: Option<u64>` to `PlanNode` (`:103`) with `#[serde(default, skip_serializing_if =
    "Option::is_none")]`. SQL parsers leave it `None`, so SQL snapshots are unchanged.
  - Add `mod mongo { pub(super) fn parse(raw: &serde_json::Value) -> Option<PlanNode> }`, dispatched from
    `parse_plan` (`:160`). The input is one row with one JSON cell holding the explain reply.
- Mapping, per the spec §6 table:
  - the root is `queryPlanner.winningPlan`, or `winningPlan.queryPlan` on 8.0 SBE;
  - stats are matched by `stage` position from `executionStats.executionStages`;
  - `examined` = `docsExamined`, or `keysExamined` for IXSCAN;
  - `detail` = `indexName`, `keyPattern` (compact), `direction`, and the filter, compacted and cut to 120 chars;
  - for an aggregation, the `stages[]` array becomes a chain: the `$cursor` plan is the deepest child, and each later
    stage wraps the previous one;
  - unknown shapes give `PlanTree::Table` with only `raw` (the JSON/Raw views).
- Modify: `crates/core/src/server.rs:184`. In the Mongo arm, `explain: true`.
- Modify: `src-tauri/src/commands/query.rs:220` (`query_explain_impl`) and `src-tauri/src/query_sessions.rs:2944`.
  Add a Command branch:
  - parse the statement at the cursor with `MongoLanguage`;
  - only `find` (with modifiers), `findOne`, `countDocuments`, `distinct` or `aggregate` are allowed, else `Explain is
    available for find, count, distinct and aggregate`;
  - build the explain through the existing `.explain()` modifier path (`parse.rs:930`), with verbosity
    `executionStats`, or `queryPlanner` when read-only;
  - run it as a Read on the tab session;
  - feed the reply to `parse_plan(ExplainFormat::MongoJson, …)`.
- Create: `crates/query/tests/fixtures/explain/mongo/`, holding replies recorded from mongo60 and mongo80. Record them
  with a throwaway script and commit only the JSON. The cases are `ixscan_find`, `collscan_find`, `sort_find` and
  `aggregate_lookup`.
- Test: `crates/query/src/explain.rs` tests (fixtures); `src-tauri/tests/mongo_driver.rs`.

**Interfaces:**
- Produces: `PlanNode.examined` (TS: `examined?: number | null`), and Mongo capabilities with `explain: true`.

- [ ] **Step 1: Record the fixtures and write failing parser tests**, for each fixture × version:
  - `mongo_ixscan`: the root chain is `FETCH` → `IXSCAN`, and the IXSCAN detail contains the index name.
  - `mongo_collscan`: the label is `COLLSCAN`, `examined` equals the doc count and `actual_rows` is set.
  - `mongo_sort`: the chain contains `SORT`.
  - `mongo_aggregate_chain`: `$lookup` wraps the `$cursor` plan.
  - `mongo_unknown_shape_falls_back`: `{"foo":1}` gives a Table with raw.
  - `sql_snapshots_unchanged`: the existing tests still pass.
- [ ] **Step 2: Run them.**
  - Command: `cargo test -p dnt-query explain`
  - Expected: FAIL.
- [ ] **Step 3: Implement the parser, the capability and the query branch.**
- [ ] **Step 4: Write and run failing DB tests** (`mongo_driver.rs`):
  - `explain_indexed_find`
  - `explain_unindexed_find_is_collscan`
  - `explain_update_refused`, with the exact message
  - `explain_read_only_uses_query_planner`: no `executionStats` in raw
- [ ] **Step 5: Regenerate the bindings.** Commit with the message `feat(mongo): Explain for find and aggregate`.

### Task 11: Explain UI for Mongo (C5)

**Files:**
- Modify: `ui/src/tabs/query/explainModel.ts:247`. The signature becomes
  `planWarnings(root: PlanNode, rules: PlanRules = SQL_RULES)`, with
  `export type PlanRules = 'sql' | 'document'`. These are rule-set names, not kind literals.
  - The document rules:
    - `COLLSCAN` with `examined > 1000` gives `Collection scan over <n> documents`;
    - a `SORT` stage gives `In-memory sort (no index supports this sort)`;
    - `examined / actual_rows > 10` gives `Examined <n> documents to return <m>`.
- Modify: `ui/src/engine/index.ts`. Add `export function planRulesOf(kind): PlanRules`: an exhaustive switch where
  mongo gives `'document'`, SQL kinds give `'sql'`, and redis gives `'sql'` (never reached).
- Modify: `ui/src/tabs/query/ExplainView.tsx:39`.
  - Pass `planRulesOf(kind)`.
  - Hide the classic view when `classic` is absent. Check that the existing code already does this.
- No new gating is needed: `capsOf` uses the server capabilities, so the Mongo `explain: true` from Task 10 turns on
  the button, the menu and Ctrl+Shift+E (`appMenus.ts:112`, `QueryToolbar.tsx:682`). Verify this with a test.
- Modify: `e2e-web/src/commandData.ts`. Fake `query_explain` for Mongo with a recorded `PlanView`.
- Test: `explainModel.test.ts`, `ExplainView.test.tsx`, `QueryToolbar.test.tsx`; `e2e-web/specs/mongo-explain.spec.ts`.

**Interfaces:**
- Consumes: `PlanNode.examined` and the Mongo `explain` capability.

- [ ] **Step 1: Write failing tests.**
  - The three document warnings fire at their thresholds and not below them.
  - SQL warnings are unchanged with the default rules.
  - The Explain button shows on a Mongo tab whose caps have `explain: true`, and is hidden on Redis.
- [ ] **Step 2: Run them.**
  - Command: `pnpm --dir ui test -- explainModel ExplainView QueryToolbar`
  - Expected: FAIL.
- [ ] **Step 3: Implement** the items in the Files list.
- [ ] **Step 4: Run the unit tests and the e2e spec.**
  - Commands: `pnpm --dir ui test`, and `pnpm --dir e2e-web test mongo-explain`. The spec presses Ctrl+Shift+E on
    `db.orders.find({status:"paid"})` and sees the tree with the COLLSCAN warning.
  - Expected: PASS.
- [ ] **Step 5: Commit.** Message: `feat(ui): Explain panel for MongoDB`.

### Task 12: Export for Mongo backend (C4)

**Files:**
- Modify: `src-tauri/src/commands/export.rs`:
  - `export_rows_impl` `:142`, `export_query_result_impl` `:268`, `export_table_impl` `:323`;
  - replace `require_sql` with an `Option<SqlKind>` from an exhaustive match on `DbKind`.
- Modify: `src-tauri/src/export_format.rs`:
  - `ExportFormat` `:48`: add `Jsonl`;
  - `open_writer` `:873`: `Sql` with `None` gives `SQL INSERT export is not available for MongoDB`.
  - The `Json` and `Jsonl` writers gain a document mode, used when the kind is Mongo:
    - each row becomes one object of its non-missing cells, in column order;
    - nested `Value::Json` cells are embedded as JSON, not as strings;
    - the output is canonical Extended JSON.
  - CSV, XLSX and Markdown write nested values as relaxed Extended JSON text.
  - A cell missing from a document writes nothing in JSON/JSONL and an empty cell elsewhere (Review Focus 4). Find how
    `rows.rs` represents an absent field, and test that representation.
- Whole-collection export: `export_table_impl` builds a `DataRequest { command: Some(CommandRead::Page …) }` with the
  view's filter and sort, unbounded rows and `no_timeout`, and streams it through the existing `stream_to_file` /
  `data_exec`. The driver streams the cursor, so no paging loop is needed. This replaces the spec's "pages of 1,000",
  which a streaming cursor makes unnecessary.
- Test: `export_format.rs` unit tests; `src-tauri/tests/mongo_driver.rs`.

**Interfaces:**
- Produces: `ExportFormat::Jsonl` (TS `'jsonl'`, or whatever serde naming the enum uses), available for every engine.

- [ ] **Step 1: Write failing tests.**
  - `mongo_json_rebuilds_documents`: a nested `{a:{b:1}}` cell comes out as an object, not a string.
  - `mongo_json_omits_missing_fields`
  - `mongo_jsonl_one_document_per_line`
  - `mongo_csv_nested_as_relaxed_ejson`
  - `mongo_sql_format_refused`
  - `sql_exports_unchanged`: the existing snapshot tests still pass.
- [ ] **Step 2: Run them.**
  - Command: `cargo test -p dntdatabase --lib export_format`
  - Expected: FAIL.
- [ ] **Step 3: Implement** the items in the Files list.
- [ ] **Step 4: Write and run failing DB tests.**
  - `export_collection_to_json_and_csv` (mongo80): this export runs as a job, filtered by `{status: "paid"}`. The file's
    document count equals `countDocuments`.
  - `export_collection_cancel`: the job ends cancelled.
- [ ] **Step 5: Regenerate the bindings.** Commit with the message `feat(mongo): export collections and results`.

### Task 13: Import for Mongo backend (C4)

**Files:**
- Modify: `crates/core/src/server.rs:184`. In the Mongo arm, `import: true`.
- Create: `crates/transfer/src/infer.rs`, holding
  `pub fn infer_field(samples: &[&str]) -> FieldType` and
  `pub enum FieldType { String, Int, Double, Bool, Date, ObjectId }`, plus
  `pub fn convert_field(text: &str, ty: FieldType) -> Result<serde_json::Value, String>`.
  - Inference never yields `ObjectId`; that type is only an override.
  - The order is: int (every sample fits i64), then double, then bool (`true` / `false`, case-insensitive), then date
    (RFC 3339 or `YYYY-MM-DD`), else string.
  - Empty samples are ignored.
  - `convert_field` emits Extended JSON wrappers (`{"$date": …}`, `{"$oid": …}`, `{"$numberLong": …}` when the value
    is outside i32).
- Modify: `crates/core/src/command.rs`. Add to `CommandSession`:
  `async fn insert_batch(&self, obj: &ObjectRef, docs: &[String], opts: &ExecOptions) -> Result<BatchOutcome,
  DbError>`, defaulting to `Unsupported`.
  - `BatchOutcome { inserted: u64, errors: Vec<BatchError> }` and `BatchError { index: usize, message: String }`.
  - Mongo parses each canonical EJSON doc and sends `insert` with `ordered: false`.
  - `writeErrors` map to `BatchError`s. Duplicate key and validation errors count; any other error is returned as
    `Err`.
- Modify: `src-tauri/src/import_data/target.rs:128` (`import_target`). Add a Command branch:
  - the target is a collection;
  - `columns` = the top-level fields from `table_detail`;
  - a new field `new_collection: bool`, true when the collection is absent from the listing.
  - A name that is not a valid collection name gives `Invalid collection name`.
- Create: `src-tauri/src/import_data/mongo.rs`, the Mongo job runner. It reads the source (`crates/transfer/src/read`)
  and builds documents:
  - CSV/XLSX: each mapped column becomes a top-level field, converted with its type (the inferred one or the user's
    override).
  - JSON/JSONL: each record is kept as-is through `serde_json` → canonical EJSON.

  Then it sends `insert_batch` in batches of 1,000.
  - Progress is reported per batch.
  - Cancel is checked between batches, and the job reports `inserted`.
  - The first 100 record errors are logged in the job detail. A conversion failure is a record error (Review
    Focus 5).
  - Dry run converts every record, logs the first 100 errors and writes nothing.
- Modify: `src-tauri/src/import_data/plan.rs` (`import_prepare` `:264`, `import_start` `:383`, `import_dry_run`
  `:468`).
  - Command branch: the verdict is class `Write` with targets `[collection]`, through the same guard as
    `command_edit_verdict`. Read-only blocks, and Production needs confirmation by the collection name.
  - `ImportRequest` gains `field_types: Vec<(String, FieldType)>`, used for overrides only.
  - `Inspection` gains `inferred: Vec<FieldType>` per source column, computed in `import_inspect` from the first 1,000
    rows.
- Test: `infer.rs` unit tests; `src-tauri/src/import_data/mongo.rs` unit tests (a fake `insert_batch`);
  `src-tauri/tests/mongo_driver.rs`.

**Interfaces:**
- Produces:
  - `FieldType` (serde lowercase: `string`, `int`, `double`, `bool`, `date`, `objectId`);
  - `Inspection.inferred`;
  - `ImportRequest.field_types`;
  - `ImportTarget.new_collection`.

  All appear in the bindings.

- [ ] **Step 1: Write failing tests.**
  - `infer`:
    - `ints`
    - `int_then_float_is_double`
    - `bools`
    - `iso_dates`
    - `mixed_is_string`
    - `blank_ignored`
    - `big_int_is_long_wrapper`
  - Runner:
    - `bad_value_in_int_column_is_record_error`: the batch still inserts the other 999.
    - `batches_of_1000`
    - `cancel_between_batches_reports_inserted`
    - `first_100_errors_logged_and_rest_counted`
    - `dry_run_writes_nothing`
    - `read_only_blocked`
    - `production_needs_confirm`
- [ ] **Step 2: Run them.**
  - Commands: `cargo test -p dnt-transfer infer`, and `cargo test -p dntdatabase --lib import_data::mongo`
  - Expected: FAIL.
- [ ] **Step 3: Implement** the items in the Files list.
- [ ] **Step 4: Write and run failing DB tests** (mongo80):
  - `import_csv_into_new_collection_with_types`
  - `import_json_round_trips_export`: the Task 12 export of `orders` imported into a new collection gives the same
    documents, compared through `find().sort({_id:1})`.
  - `import_counts_duplicate_keys`
- [ ] **Step 5: Regenerate the bindings.** Commit with the message `feat(mongo): import CSV, XLSX and JSON into a collection`.

### Task 14: Export and import UI for Mongo (C4)

**Files:**
- Modify: `ui/src/engine/index.ts`.
  - Mongo caps: `import: true`, matching the server.
  - Add `export function exportFormatsOf(kind): ExportFormat[]`: an exhaustive switch where Mongo gets everything
    except `sql`, and SQL kinds get all formats.
- Modify: `ui/src/dialogs/resultExport.ts`.
  - `FORMATS` `:37` gains `{format: 'jsonl', label: 'JSON lines', ext: 'jsonl', …}`.
  - Remove the Command-family block at `:117`, so Mongo results export. Redis stays blocked through
    `exportFormatsOf`, which returns `[]` for redis, so the menu item is hidden.
- Modify: `ui/src/dialogs/ResultExportDialog.tsx`, which filters `FORMATS` by `exportFormatsOf(kind)`. Re-enable
  `Export…` on Mongo data views (`DataView.tsx`, `dataExportRequest`) and query results (`ResultPanel.tsx`).
- Modify: `ui/src/transfer/import/steps/TargetStep.tsx:33`. For a Mongo connection:
  - the target is an existing collection, or `New collection` with a name input;
  - it shows `Will be created` when `new_collection`.
- Modify: the import mapping step under `ui/src/transfer/import/steps/`. For a Mongo target with a CSV/XLSX source,
  each column gets a field name input (default: the source header) and a type select. The select defaults to
  `inferred`, and the options are `string`, `int`, `double`, `bool`, `date`, `objectId`.
- Modify: `ui/src/components/TreeContextMenu.tsx`. Add `Import data…` on a collection, gated on `caps.import`.
- Modify: `e2e-web/src/commandData.ts`. Fake the Mongo `import_target`, `import_inspect.inferred`, `import_dry_run`
  and `import_start`.
- Test: `resultExport.test.ts`, `ResultExportDialog.test.tsx`, `TargetStep.test.tsx`, the mapping step test,
  `TreeContextMenu.test.tsx`; `e2e-web/specs/mongo-transfer.spec.ts`.

**Interfaces:**
- Consumes: `ExportFormat::Jsonl` (Task 12), plus `FieldType`, `Inspection.inferred`, `ImportRequest.field_types` and
  `ImportTarget.new_collection` (Task 13).

- [ ] **Step 1: Write failing tests.**
  - Export:
    - the Mongo dialog has no `SQL INSERT` and has `JSON lines`;
    - the SQL dialog is unchanged apart from the added `JSON lines`;
    - Mongo data view `Export…` is enabled;
    - Redis is still hidden.
  - Import:
    - the Mongo target step offers `New collection`;
    - type selects default to the inferred type, and an override is sent in `field_types`;
    - the tree menu has `Import data…` on a collection and not on a read-only connection.
- [ ] **Step 2: Run them.**
  - Command: `pnpm --dir ui test -- resultExport ResultExportDialog TargetStep TreeContextMenu`
  - Expected: FAIL.
- [ ] **Step 3: Implement** the items in the Files list.
- [ ] **Step 4: Run the unit tests and the e2e spec.**
  - Commands: `pnpm --dir ui test`, and `pnpm --dir e2e-web test mongo-transfer`. The spec exports a collection to JSON
    lines, then imports a CSV into a new collection, and the job finishes with the inserted count.
  - Expected: PASS.
- [ ] **Step 5: Commit.** Message: `feat(ui): export and import for MongoDB`.

### Task 15: Final verification

- [ ] **Step 1:**
  - `cargo fmt --all -- --check`
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - `$env:DNT_TEST_DB='1'; cargo test --workspace --no-fail-fast -- --test-threads=1`
  - `pnpm --dir ui lint`, `pnpm --dir ui test`
  - `$env:E2E_WEB_PORT='5176'; pnpm --dir e2e-web test`

  Expected: all pass. Re-run known timing-flaky SQL cancel tests on their own before calling a failure real.
- [ ] **Step 2:** Do a whole-branch review of `main...mongo-tools` by a fresh reviewer, then fix anything it finds.
