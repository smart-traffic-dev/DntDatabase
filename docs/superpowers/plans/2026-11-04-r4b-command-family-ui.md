# R4b — Command family in the UI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The UI handles a Command connection (Mongo or Redis) end to end against the R4a backend: create it, browse its tree, run console scripts, view and edit data, and browse and edit keys. Menus show only what the connection's capabilities allow. No UI code compares a kind with a literal outside `ui/src/engine/`.

**Architecture:**
- **One engine module:** `ui/src/engine/` holds every per-kind and per-family decision as an exhaustive `switch` ending in `assertNever`. An ESLint rule keeps kind literals out of every other file.
- **Capabilities, not kinds:** features are gated with `capsOf(serverInfo)`. Before connect, `defaultCaps(kind)` stands in.
- **Reuse:** the Command family uses the existing query tab, run events, confirm dialog, data view, `EditPreviewDialog` and History. Only the key browser is a new tab kind.
- **Proof without a real driver:** the e2e-web fake backend gains a Redis and a Mongo connection.

**Tech Stack:** React 19, TypeScript, zustand, Monaco (Monarch tokenizers), vitest + Testing Library, Playwright (e2e-web). Small Rust changes in `crates/core`, `crates/testkit` and `src-tauri` (Tasks 4 and 7).

**Spec:** `docs/superpowers/specs/2026-10-31-non-sql-driver-core-design.md`. This plan covers §5.4, §6.6, §7.1 (UI + ESLint), §7.2 (UI), §7.3, §7.5, §7.6 and §7.7, which is R4 in §8. Also read the R4 sections of `2026-10-31-r1-followups.md`, `2026-10-31-r2-followups.md` and `2026-10-31-r3-followups.md`, and the R4a plan `2026-11-02-r4a-command-app-backend.md` for the backend commands this UI calls.

## Global Constraints

- **MySQL/PostgreSQL behaviour and look stay unchanged.** Existing vitest and e2e-web specs pass. They change only where a test asserted a literal kind comparison that moved into `ui/src/engine/`.
- **All UI copy is English.** Exact strings below are verbatim.
- **No new npm package or crate.** Monaco languages are Monarch definitions registered in code.
- **Exhaustive switches:** each function in `ui/src/engine/` switches on `DbKind` or `EngineFamily` with a `default: return assertNever(x)`. Adding a fifth kind must break the TypeScript build.
- **ESLint rule (spec §7.1):** `no-restricted-syntax` bans `BinaryExpression[operator=/^[!=]==?$/]` whose one side is a `Literal` with value `'mysql' | 'postgres' | 'mongo' | 'redis'`. Message: `Compare engine kinds only inside ui/src/engine/ (use familyOf/capsOf or a switch there).` It is off for `src/engine/**` and `**/*.test.{ts,tsx}`.
- **The UI hides; the backend decides.** Hidden menu items are a convenience only. Do not add UI-only safety checks that the backend lacks.
- **Bindings:** they change only in Tasks 4, 7 and 8. After a change, run `cargo run -p dntdatabase --example export_bindings` and `cargo test -p dntdatabase --test commands bindings_are_current`.
- **Verification per task:** `pnpm --dir ui lint`, `pnpm --dir ui test`, and the task's e2e-web spec with `$env:E2E_WEB_PORT='5176'; pnpm --dir e2e-web test <spec>`. For tasks that touch Rust, also run `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings` and the touched crates' tests.
- **Start point:** branch from `main` in a worktree. `main` has uncommitted UI work (ConnectionTree, RightSidebar, ResultGrid, designer grids); it must be committed or stashed by its owner first. Do not touch it.
- **Commit style:** `feat(ui|app|core|e2e-web): …`, one commit per task.
- **Decisions this plan makes:**
  1. The engine picker lists `availableEngines()`. A build without a Mongo or Redis driver does not offer that card. An existing saved connection of such a kind still opens in the dialog, can be edited and saved, and shows the banner `This build has no <MongoDB|Redis> driver yet. You can edit and save the connection, but not connect.` Test and Connect are disabled for it.
  2. New Mongo/Redis connections default to SSL mode `Disable` (R3 open item). Choosing `Prefer` shows the hint `MongoDB and Redis have no "prefer" mode: TLS is used without certificate checks.`
  3. Switching the engine resets `options.driver` to the new kind's default: `none`, `{kind:'mongo', srv:false, auth_source:null, replica_set:null, direct_connection:false, extra_hosts:[]}` or `{kind:'redis', db:0}`.
  4. `DatabaseInfo` gains `key_count: number | null` (Rust `Option<u64>` with `#[specta(type = Option<f64>)]`, `#[serde(default)]`). SQL drivers return `None`, and the fake returns its db sizes. The tree labels a `keyspace-db` node `db0 · 1,234 keys`, or plain `db0` when the count is `null`.
  5. Command tabs get statement ranges from the backend `statementRanges(text, dialect, connId)`, debounced 150 ms. `dialect` is ignored when the connection has a language; pass `rangesDialect(kind)`, which returns `{ engine: 'postgres' }` for the Command family. The local `splitRanges` stays SQL-only.
  6. The `Table` / `JSON` toggle appears on results of kinds where `hasDocumentView(kind)` is true (Mongo only). The JSON view rebuilds each row as an object keyed by column, skipping `Null` cells that came from field-union padding. Spec §7.3 leaves this rendering open. A cell that is `Null` because the document has `field: null` cannot be told apart and is also skipped. This is a known limitation, documented in the component.
  7. `CellInput` gains `{ kind: 'unset' }` (Rust `CellInput::Unset`), which is valid only for the Command family. A SQL `build_edits` that receives it returns `EditError` `Removing a field is not available for this engine`. The data-view cell menu shows `Remove field` only when the family is Command. The fake maps `Unset` on `value` to `HDEL`.
  8. The key browser is a new tab kind `'keys'` with `{ connectionId, database }`. The tab identity is `keys:<connectionId>:<database>`. The selected key is held as `KeyName` and compared by `raw`, never by `display`.
  9. Command AI suggestions (F2/F3 follow-ups) stay out of scope. `editorLanguage` returns the Monaco id, and the AI provider keeps its SQL-only language list.

## Review Focus

1. **A saved Redis connection opened in a build without a Redis driver.** The dialog must not offer Test or Connect, and the tree's Connect must show the friendly message instead of a raw `no driver registered for Redis`. Pinned by Task 3 tests `saved_command_conn_without_driver_is_editable` and `tree_connect_without_driver_is_friendly`.
2. **A binary Redis key whose `display` equals another key's.** Selecting, editing and deleting must act on the key the user clicked. Pinned by Task 8 test `keys_with_same_display_stay_distinct`.
3. **Pasting a `mongodb+srv://` URI and then picking an SSH tunnel.** The dialog shows the backend's `SSH tunnel needs a single host` on the SSH field, not a generic error. Pinned by Task 3 test `srv_with_ssh_shows_field_error`.
4. **A Mongo result whose later batch adds a field.** Earlier rows show the new column empty in Table view, and the JSON view does not invent `null` for the missing field. Pinned by Task 6 test `json_view_skips_padding_nulls`.
5. **A key deleted from another client while it is open in the key browser.** Refresh shows `Key no longer exists` and leaves the list usable. Pinned by Task 8 test `vanished_key_shows_notice`.

---

### Task 1: `ui/src/engine/` and the ESLint rule

**Files:**
- Create: `ui/src/engine/index.ts`, `ui/src/engine/assertNever.ts`, `ui/src/engine/engine.test.ts`
- Modify: `ui/eslint.config.js`
- Modify: every non-test file with a kind literal comparison, plus the 7 `sqlEngine(` callers. On 2026-11-04 there were 74 such lines in 37 files. The new ESLint rule lists them all; the largest are `query/completion/context.ts`, `store/completion.ts`, `tabs/designer/columnModel.ts`, `ConnectionDialog.tsx`, `TreeContextMenu.tsx`, `DataView.tsx` and `store/tree.ts`.
- Delete: `ui/src/query/engine.ts`; its `Engine` type and `sqlEngine` move into `ui/src/engine/`

**Interfaces:**
- Produces (all in `ui/src/engine/index.ts`):

```ts
export type SqlEngine = Dialect['engine'];                         // 'mysql' | 'postgres'
export function assertNever(x: never): never;                       // throws Error(`unexpected value ${String(x)}`)
export function familyOf(kind: DbKind): EngineFamily;
export function sqlEngine(kind: DbKind): SqlEngine | null;         // null for the Command family — no MySQL fallback
export function defaultCaps(kind: DbKind): Capabilities;           // matches the Rust drivers: sql_full for SQL; spec §3.1 for Mongo/Redis
export function capsOf(info: ServerInfo | null | undefined, kind: DbKind): Capabilities; // info?.capabilities ?? defaultCaps(kind)
export function quoteIdent(kind: DbKind, name: string): string;     // SQL: today's quoting; Command: name unchanged
export function editorLanguage(kind: DbKind): 'mysql' | 'pgsql' | 'mongo' | 'redis';
export function engineChip(kind: DbKind): 'MY' | 'PG' | 'MG' | 'RD';
export function engineLabel(kind: DbKind): string;                  // 'MySQL / MariaDB' | 'PostgreSQL' | 'MongoDB' | 'Redis'
export function namespaceLabel(kind: DbKind): 'Database' | 'Schema'; // what the tree calls a namespace, for CreateNamespaceDialog
export function hasDocumentView(kind: DbKind): boolean;             // mongo only
export function rangesDialect(kind: DbKind): Dialect;               // decision 5
export function isKind(kind: DbKind, k: DbKind): boolean;           // escape hatch for SQL-only code that already holds a SqlEngine; prefer a switch
```

- `defaultCaps` values: Mongo is `{ family:'command', namespaces:'db', transactions:false, data_edit:true, paged_objects:false, … everything else false }`. Redis is the same with `namespaces:'numbered_db'` and `paged_objects:true`. Mongo's `transactions` stays `false` until a replica set reports otherwise.
- Callers that are SQL-only (designer, DDL edit, completion, import, dump) take a `SqlEngine` obtained once at their entry point from `sqlEngine(kind)`. When it is `null`, the entry point shows nothing or throws, as below; it never guesses MySQL. This removes the R1 fallbacks in `ColumnInspector.tsx:89`, `ColumnsGrid.tsx:168` and `newQuery.ts:64`.

- [ ] **Step 1: Write the failing tests** in `engine.test.ts`:
  - `every_kind_has_a_family`: `familyOf` over `['mysql','postgres','mongo','redis']` gives `sql, sql, command, command`.
  - `sql_engine_has_no_fallback`: `sqlEngine('redis') === null` and `sqlEngine('postgres') === 'postgres'`.
  - `default_caps_match_spec`: `defaultCaps('redis').paged_objects === true`, `defaultCaps('mongo').designer === false`, and `defaultCaps('mysql')` equals the test fixture `sqlCaps` (`ui/src/test/fixtures/caps.ts`).
  - `caps_prefer_server_info`
  - `chips_and_languages`: `engineChip('mongo') === 'MG'`, `editorLanguage('redis') === 'redis'`, `editorLanguage('postgres') === 'pgsql'`.
  - `eslint_rule_flags_kind_literals`: run ESLint's `Linter` on the string `if (c.kind === 'redis') {}` with the project config. Expect one message with the rule text above. The same string under a virtual path in `src/engine/x.ts` gives none.
- [ ] **Step 2: Run** `pnpm --dir ui test engine`. Expected: FAIL (module missing).
- [ ] **Step 3: Implement** the module and the rule. Then migrate every flagged line. Each comparison becomes a call to one of the functions above, or a `switch` on `sqlEngine(...)` in SQL-only code. Keep behaviour identical for MySQL and PostgreSQL. Drop `Engine` from `query/engine.ts` and update imports to `../engine`.
- [ ] **Step 4: Run** `pnpm --dir ui lint` (zero `no-restricted-syntax` hits), `pnpm --dir ui test`, and the full e2e-web suite. Expected: PASS.
- [ ] **Step 5: Commit** `refactor(ui): engine module; no kind literals outside it`.

### Task 2: Capability gates in menus and toolbars

**Files:**
- Modify: `ui/src/components/TreeContextMenu.tsx` (design / new table / import / generate data / object actions / DDL edit)
- Modify: `ui/src/transfer/backup/backupMenu.ts` (Dump table…, Dump…, Restore…)
- Modify: `ui/src/components/appMenus.ts` (Explain), `ui/src/tabs/query/QueryToolbar.tsx` (Explain button, Manual transaction item, Commit/Rollback)
- Modify: `ui/src/tabs/ObjectTab.tsx` (DDL tab / designer entry points)
- Create: `ui/src/engine/useCaps.ts`: `useCaps(connectionId: string | null): Capabilities | null`, which reads the tree store's `ServerInfo` and the config kind and returns `capsOf(info, kind)`. It returns `null` without a connection.
- Test: `ui/src/components/TreeContextMenu.test.tsx`, `ui/src/tabs/query/QueryToolbar.test.tsx`

**Interfaces:**
- Consumes: `capsOf` and `defaultCaps` from Task 1.
- The flag for each item:

| Item | Flag |
|---|---|
| Design table, New table… | `designer` |
| Edit DDL… | `ddl_edit` |
| Object actions (truncate, drop, rename…) | `object_actions` |
| Import data… | `import` |
| Dump table…, Dump…, Restore… | `dump` |
| Generate data… | `seed` |
| Explain (menu, toolbar, shortcut) | `explain` |
| Manual transaction, Commit, Rollback | `transactions && family === 'sql'` |

- The item is removed, not disabled. Disabled items stay disabled for their existing reasons (read-only and similar).

- [ ] **Step 1: Write the failing tests:**
  - `redis_tree_menu_has_no_sql_items`: the context menu of a Redis database node contains none of the table's labels.
  - `mysql_tree_menu_unchanged`: snapshot the labels for a MySQL table node before the change, and assert that list.
  - `command_toolbar_hides_explain_and_manual_tx`
  - `caps_before_connect_use_defaults`: no `ServerInfo` for a Mongo connection gives `defaultCaps('mongo')`.
- [ ] **Step 2: Run** them. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** lint, `pnpm --dir ui test` and e2e-web. Expected: PASS.
- [ ] **Step 5: Commit** `feat(ui): gate menus and toolbars by capabilities`.

### Task 3: e2e-web fake Command connections and the connection dialog

**Files:**
- Modify: `e2e-web/src/backend.ts`:
  - `available_engines` returns all four kinds;
  - `infoOf` gives `Redis` / `Mongo` `ServerInfo` with `defaultCaps`-equal capabilities;
  - a small in-memory Redis store: db `0` holds `user:1` (hash), `queue` (list), `tags` (set), `rank` (zset), `greeting` (string), and db `2` holds 3 keys. Mongo db `shop` holds collection `orders` with 3 documents;
  - `list_databases` per kind.

  Later tasks add the rest of the handlers.
- Modify: `ui/src/dialogs/ConnectionDialog.tsx`, `ui/src/dialogs/connectionDefaults.ts`
- Modify: `ui/src/store/tree.ts` (connect error mapping)
- Test: `ui/src/dialogs/ConnectionDialog.test.tsx`; e2e `e2e-web/specs/command-connections.spec.ts` (new)

**Interfaces:**
- `connectionDefaults.ts`:
  - `ENGINES` covers all four kinds: MongoDB on port 27017 and Redis on 6379, both with user `''`.
  - `engineInfo(kind)` uses an exhaustive switch, with no MySQL fallback.
  - `defaultSsl(kind)` is `Disable` for the Command family and today's value for SQL.
  - `defaultDriverOptions(kind): DriverOptions` gives the values from decision 3.
  - `defaultPort(kind, srv: boolean)` is `null` when `srv`.
- Dialog fields by kind, from `familyOf` and the `options.driver` variant:
  - **Mongo:**
    - `Connection string`: a paste fills the fields through `parseConnectionString`, the same path as today;
    - `SRV` (checkbox; hides Port);
    - `Auth database` (`auth_source`);
    - `Replica set`;
    - `Additional hosts`: a list of `host:port` rows with `Add host` and `Remove`. It is hidden when SRV is on.
  - **Redis:** `Database index` (0–15 spinner; the backend accepts any `u16`).
  - **Hidden for the Command family:** `Charset` (MySQL-only today) and every MySQL/PG-only option. Username is labelled `Username (optional)`.
- **Removed:** the three refusals at `ConnectionDialog.tsx:354-357`, `:365-367` and `:1177-1181`, and their constant.
- **Engine cards:** come from `availableEngines()` (decision 1). They are fetched once per dialog open. On failure, fall back to the SQL kinds.
- **Field errors:** `AppError` field `options.driver.*` and `ssh` map onto the matching inputs, such as `ssh` → the SSH section.
- **Tree Connect without a driver:** `tree.ts` maps an open error whose message starts with `no driver registered for` to `This build has no <engineLabel> driver yet.`

- [ ] **Step 1: Write the failing tests** (vitest):
  - `engine_cards_follow_available_engines`: a backend returning `['mysql','postgres','redis']` shows 3 cards, without MongoDB.
  - `mongo_fields_and_srv_hides_port`
  - `redis_database_index_saved_in_driver_options`: saving gives `options.driver = {kind:'redis', db: 3}`.
  - `switching_engine_resets_driver_options`
  - `command_kinds_default_ssl_disable`
  - `srv_with_ssh_shows_field_error`: the mock `create_connection` rejects with field `ssh` and message `SSH tunnel needs a single host`. The message renders under the SSH section.
  - `saved_command_conn_without_driver_is_editable`: with `availableEngines` = SQL only, editing a saved Redis connection shows the banner from decision 1, Save is enabled, and Test is disabled.
  - `pasted_redis_uri_fills_fields`: `rediss://u@h:6380/2` sets the engine to Redis, port 6380, db 2 and SSL `Require`.
  - `tree_connect_without_driver_is_friendly` (in `tree.test.ts`)
- [ ] **Step 2: Run** them. Expected: FAIL.
- [ ] **Step 3: Implement** the dialog and the fake.
- [ ] **Step 4: Write the e2e spec** `command-connections.spec.ts`: create a Redis connection and a Mongo connection through the dialog, then see both in the tree with chips `RD` / `MG`. Run it. Expected: PASS.
- [ ] **Step 5: Commit** `feat(ui): MongoDB and Redis connections in the dialog`.

### Task 4: Tree for the Command family

**Files:**
- Modify (Rust): `crates/core/src/meta.rs` or wherever `DatabaseInfo` lives (`key_count`), `crates/testkit/src/fake_command.rs` (fill `key_count`), the MySQL/PG introspectors (`key_count: None`), `ui/src/bindings.ts` (regenerated)
- Modify: `ui/src/store/treeModel.ts`, `ui/src/store/tree.ts`, `ui/src/components/ConnectionTree.tsx` (render and open `keyspace-db`)
- Modify: `e2e-web/src/backend.ts` (`list_databases` key counts, `list_objects(Collection)`)
- Test: `ui/src/store/tree.test.ts`, `crates/testkit` test, e2e `e2e-web/specs/command-tree.spec.ts` (new)

**Interfaces:**
- Rust: `pub struct DatabaseInfo { pub name: String, #[serde(default)] #[specta(type = Option<f64>)] pub key_count: Option<u64> }`. The bindings change only by this field.
- `treeModel.ts`:
  - `NodeKind` gains `'keyspace-db'`.
  - `engineOf` is replaced by `engineChip(kind)`.
  - `folderKindsOf(kind, caps)` is an exhaustive switch:
    - `db_schema` → schema nodes, as today;
    - `db` + `sql` → today's folders;
    - `db` + `command` → `['Collection', 'View']` (labels `Collections`, `Views`);
    - `numbered_db` → no folders.
- `tree.ts`: with `numbered_db`, each database becomes a `keyspace-db` leaf labelled by decision 4. Activating it (double-click or Enter) calls `openKeysTab(connectionId, database)` from Task 8. Until Task 8 lands, this is a stub exported from `store/tabs.ts` that is a no-op behind a `TODO(R4b Task 8)` comment, which Task 8 deletes.
- Number format: `toLocaleString('en-US')`.

- [ ] **Step 1: Write the failing tests:**
  - `redis_databases_are_keyspace_leaves`: labels are `db0 · 5 keys` and `db2 · 3 keys`, and the nodes have no expander.
  - `null_key_count_shows_plain_name`
  - `mongo_database_has_collections_folder`: expanding `shop` gives `Collections`, `Views`, and `orders` under Collections.
  - `sql_tree_unchanged`
  - Rust `fake_reports_key_counts`
- [ ] **Step 2: Run** them. Expected: FAIL.
- [ ] **Step 3: Implement.** Regenerate bindings.
- [ ] **Step 4: Run** the Rust tests for `dnt-core`, `dnt-testkit` and `dntdatabase`, plus `bindings_are_current`, lint, `pnpm --dir ui test` and the e2e spec `command-tree.spec.ts` (Redis tree labels, Mongo collections). Expected: PASS.
- [ ] **Step 5: Commit** `feat(ui): tree for Command connections`.

### Task 5: Query tab for the Command family

**Files:**
- Create: `ui/src/engine/monaco/mongo.ts`, `ui/src/engine/monaco/redis.ts` (Monarch definitions plus `registerCommandLanguages(monaco)`), and `ui/src/engine/monaco/languages.test.ts`
- Modify: `ui/src/tabs/query/SqlEditor.tsx` (language from `editorLanguage`, register once), `ui/src/tabs/query/QueryTab.tsx` (no `?? 'mysql'` fallback), `ui/src/query/completion/monacoProvider.ts` (register SQL completion only for `mysql` / `pgsql`; ranges for Command tabs from the backend)
- Modify: `ui/src/store/queryTabs.ts` (statement ranges per family)
- Modify: `ui/src/dialogs/RunConfirmDialog.tsx` (`admin` label `Admin`, already in the `LABEL` record; make sure it renders), `ui/src/tabs/query/editorCommands.ts` (Format SQL hidden for the Command family)
- Modify: `e2e-web/src/backend.ts`: `statement_ranges` splits by line for Redis and by `;` or blank line for Mongo. `query_plan` / `query_run` for Command connections cover `GET`, `SET`, `HGETALL`, `FLUSHDB`, `db.orders.find({})` and `db.orders.deleteMany({})`, with labels, verdicts and events in the R4a shapes.
- Test: `ui/src/tabs/query/QueryTab.test.tsx`; e2e `e2e-web/specs/command-query.spec.ts` (new)

**Interfaces:**
- **Monarch tokens (spec §7.3):**
  - `mongo`:
    - keyword `db`;
    - method names after `.` (`find`, `insertOne`, …, as `type.identifier`);
    - `$…` operators (`keyword.operator`);
    - single, double and back-quoted strings;
    - numbers;
    - `//` and `/* */` comments;
    - `ObjectId`, `ISODate` (`predefined`).
  - `redis`:
    - the first word on a line is a `keyword`, case-insensitive;
    - double- and single-quoted strings with `\` escapes;
    - numbers;
    - `#` line comments.
- **Ranges:** for `familyOf(kind) === 'command'`, `queryTabs` calls `api.statementRanges(text, rangesDialect(kind), connectionId)`. The call is debounced by 150 ms, and a newer call supersedes an older one: drop stale responses by sequence number. The SQL path is unchanged.
- **The run, plan and confirm flow is unchanged.** `PlanView.family` is used only to hide SQL-only UI: the implicit-commit notice and the estimate column.

- [ ] **Step 1: Write the failing tests:**
  - `mongo_tokens`: `monaco.editor.tokenize` on `db.orders.find({ $gt: 1 }) // c` gives the token types listed above in order. If `tokenize` is unavailable in jsdom, unit-test the Monarch definition with a small state-machine driver in the test file.
  - `redis_first_word_is_keyword`
  - `command_tab_uses_backend_ranges`: typing `GET a\nSET b 1` calls `statementRanges` once after 150 ms with the connection id, and the gutter shows 2 ranges.
  - `stale_range_response_is_dropped`
  - `command_tab_has_no_sql_completion`
  - `run_confirm_shows_admin_label`
- [ ] **Step 2: Run** them. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Write and run the e2e spec** `command-query.spec.ts`:
  - in a Redis tab, `SET a 1` then `GET a` gives a result grid with `value` = `1`;
  - `FLUSHDB` on a Production-tagged connection opens the confirm dialog and asks for the name;
  - the History panel lists both statements.

  Expected: PASS.
- [ ] **Step 5: Commit** `feat(ui): query tabs for MongoDB and Redis`.

### Task 6: Table / JSON toggle for document results

**Files:**
- Create: `ui/src/tabs/query/DocumentView.tsx`, `ui/src/tabs/query/documents.ts` (row → document), and `ui/src/tabs/query/documents.test.ts`
- Modify: `ui/src/tabs/query/ResultGrid.tsx` (toggle in the result header; keep the uncommitted local edits on `main` intact — rebase onto them)
- Test: `ui/src/tabs/query/ResultGrid.test.tsx`; extend `command-query.spec.ts`

**Interfaces:**
- `export function rowToDocument(columns: string[], row: Value[], present: boolean[] | null): Record<string, unknown>`. `present[i] === false` marks a padding cell (decision 6). When `present` is `null`, every non-`Null` cell is included and `Null` cells are skipped. Each `Value` is converted to relaxed EJSON:
  - `Json` → parsed;
  - `Other{type_name:'ObjectId', text}` → `{ "$oid": text }`;
  - other `Other` values → `{ "$<type_name>": text }`;
  - `DateTime` → `{ "$date": iso }`;
  - `Bytes` → `{ "$binary": { base64, subType: "00" } }`;
  - `Uuid` → `{ "$uuid": … }`;
  - scalars → JSON scalars.
- The toggle has two buttons, `Table` and `JSON`, with `aria-pressed`. It is shown only when `hasDocumentView(kind)`. The choice is held per result in tab state and is not persisted.
- `DocumentView` lists documents with 2-space indent, read-only, with a copy button per document.

- [ ] **Step 1: Write the failing tests:**
  - `object_id_to_oid`
  - `json_view_skips_padding_nulls`: columns `_id, a, b`; row 1 has `b` as padding and row 2 has `a` as padding. Expect the documents `{_id, a}` and `{_id, b}`.
  - `toggle_only_for_document_kinds`: Redis results and MySQL results show no toggle.
- [ ] **Step 2: Run** them. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the tests and the extended e2e (Mongo `db.orders.find({})` → JSON view shows 3 documents). Expected: PASS.
- [ ] **Step 5: Commit** `feat(ui): JSON view for document results`.

### Task 7: Data view and edits for the Command family

**Files:**
- Modify (Rust): `crates/core/src/edit.rs` (`CellInput::Unset`), `crates/testkit/src/fake_command.rs` (`Unset` on `value` → `HDEL`), the SQL `build_edits` paths (refuse `Unset` with the decision 7 message), `ui/src/bindings.ts` (regenerated)
- Modify: `ui/src/store/tableData.ts` (`draftToFilter` builds `raw_json` for the Command family), `ui/src/tabs/FilterBar.tsx` (raw mode label and placeholder per family), `ui/src/tabs/DataView.tsx` (kind checks via the engine module; cell menu `Remove field`)
- Modify: `ui/src/tabs/data/EditPreviewDialog.tsx`: the `KINDS` counts (`UPDATE` / `INSERT` / `DELETE`) are by SQL prefix. For the Command family, show only the total `N commands`.
- Modify: `e2e-web/src/backend.ts` (`table_data`, `table_row_count`, `data_editable`, `data_preview`, `data_apply` for Mongo `orders`)
- Test: `ui/src/store/tableData.test.ts`, `ui/src/tabs/DataView.test.tsx`, Rust tests in `crates/core` and `crates/testkit`; e2e `e2e-web/specs/command-data.spec.ts` (new)

**Interfaces:**
- Rust: `CellInput::Unset`, serde `{ "kind": "unset" }`, with a doc comment: `Removes the field (Command family only).`
- **Filter bar:**
  - raw mode on the Command family is labelled `JSON filter`, with placeholder `{ "status": "paid" }`;
  - it sends `raw_json`, never `raw_where`;
  - the SQL family is unchanged (`WHERE`).
  - Backend errors with field `filter` show under the input, as raw WHERE errors do today.
- **Non-atomic banner:** when `data_editable.atomic === 'no'`, the staged-changes bar shows `Changes are applied one document at a time.`
- **Production:** the backend refusal message (R4a Task 6) renders in the existing preview error banner. There is no UI-side check.

- [ ] **Step 1: Write the failing tests:**
  - Rust `unset_round_trips_serde`, `sql_build_edits_refuse_unset`, `fake_unset_deletes_field`
  - `command_raw_filter_sends_raw_json`
  - `sql_raw_filter_unchanged`
  - `remove_field_only_for_command_family`
  - `command_preview_counts_commands`: two staged edits give `2 commands`.
  - `not_atomic_banner_text`
- [ ] **Step 2: Run** them. Expected: FAIL.
- [ ] **Step 3: Implement.** Regenerate bindings; the only change is the `CellInput` variant.
- [ ] **Step 4: Run** the Rust tests, `bindings_are_current`, lint, `pnpm --dir ui test` and the e2e spec `command-data.spec.ts`: open `orders`, filter `{"status":"paid"}` and see 1 row, edit a cell, preview shows `updateOne…`, apply, and the grid refreshes. Expected: PASS.
- [ ] **Step 5: Commit** `feat(ui): data view and edits for the Command family`.

### Task 8: Key browser tab

**Files:**
- Create: `ui/src/tabs/keys/KeysTab.tsx` (layout), `KeyList.tsx` (left pane), `KeyValuePane.tsx` (right pane, per-type editors), `keyEdits.ts` (build `KeyEdit` values), `ui/src/store/keysTabs.ts` (zustand), `ui/src/tabs/keys/keys.css`
- Modify: `ui/src/store/tabs.ts` (`KeysTabRef`, `openKeysTab`; remove the Task 4 stub), the tab host that renders tabs by kind, tab persistence (`tabs_save` payload: keys tabs are restored like object tabs)
- Modify (Rust): `crates/core/src/error.rs` (`KeyNotFound`), `crates/core/src/keyspace.rs` (doc comment), `crates/testkit/src/fake_command.rs`, the `DbError` → `AppError` mapping in `src-tauri`, `ui/src/bindings.ts` if it changes
- Modify: `e2e-web/src/backend.ts` (`keys_scan`, `key_value`, `key_edit_preview`, and `data_apply` for key plans)
- Test: `ui/src/tabs/keys/KeysTab.test.tsx`, `ui/src/store/keysTabs.test.ts`; e2e `e2e-web/specs/key-browser.spec.ts` (new)

**Interfaces:**
- Consumes: `keysScan`, `keyValue`, `keyEditPreview`, `dataApply`, `EditPreviewDialog` and `useCaps`.
- `export interface KeysTabRef { kind: 'keys'; id: string; connectionId: string; database: string; title: string }`. `title` is `<connection name> · db<n>`.
- `export function openKeysTab(connectionId: string, database: string): void`. It focuses an existing tab with the same id (decision 8).
- **Store state per tab:**
  - `pattern` (default `*`);
  - `keyType: KeyType | null`;
  - `keys: KeySummary[]`;
  - `cursor: string | null`;
  - `done: boolean`;
  - `selected: KeyName | null`;
  - `value: KeyValueView | null`;
  - `range: ValueRange` (`{offset:0,count:200}`);
  - `error`;
  - `pending: PendingApply | null`.
- **Left pane:**
  - pattern input (Enter rescans from cursor `null`);
  - type select (`All types`, `String`, `Hash`, `List`, `Set`, `Sorted set`, `Stream`, `JSON`);
  - the list shows display name, a type badge and a TTL (`no TTL` or a humanized `12m 5s`);
  - the footer shows `Load more` while `next_cursor` is set, and the count `N keys loaded` (the `keyspace-db` total from the tree when known: `N of 1,234 keys loaded`);
  - `count` is 200 per scan.
- **Right pane** (spec §7.6):
  - string: text editor with modes `Text` / `JSON` / `Hex`; `Save` sends `set_string`;
  - hash: grid `field | value` with `Add field`, inline edit (`hash_set`) and delete (`hash_delete`);
  - list: grid `index | value` with `Push head`, `Push tail`, edit (`list_set`) and remove (`list_remove` count 1);
  - set: list with `Add member` / remove;
  - zset: `member | score` with add and remove;
  - stream: read-only `id | fields`;
  - other: read-only text.
  - The header shows the type, TTL (with `Set TTL…` / `Persist` → `expire`), `memory_bytes` (formatted `1.2 KB`, hidden when null), `Rename…` (`rename`), `Delete key` (`delete`) and `Refresh`.
  - When `total` exceeds the loaded items, the pane shows `Load more` (next `range.offset`).
- **Toolbar:** `New key…` opens a dialog with name, type (String, Hash, List, Set, Sorted set) and a first value. It sends `set_string`, `hash_set`, `list_push`, `set_add` or `z_set_add`.
- **Edit flow:**
  1. Each action builds one `KeyEdit`, then calls `keyEditPreview(id, db, edit)`.
  2. `EditPreviewDialog` shows the result. It already shows statement text, verdict and confirm name.
  3. Confirm calls `dataApply(plan_id, true)`.
  4. On success, the key's value reloads. On `delete` or `rename`, the list updates in place.
  5. A Block verdict shows the reason and disables Confirm, as the dialog already does.
- **Edit gating:** without `caps.data_edit`, or on a read-only connection, the edit controls are hidden and the pane is read-only.
- **Encoding:** values and fields typed as text are encoded to `KeyBytes` with UTF-8 → base64. In the Hex mode, input is parsed as hex pairs; invalid hex disables `Save` with `Invalid hex`.
- **Vanished key:** when `keyValue` fails with `AppError.kind === 'key_not_found'`, show `Key no longer exists`. Drop the key from the list and clear the selection. No such error exists yet: the fake answers `Server "ERR no such key …"`. This task adds `DbError::KeyNotFound { key: String }` in `crates/core/src/error.rs`, with display text `Key no longer exists` and AppError kind `key_not_found` in the existing `DbError` → `AppError` mapping. It documents on `Keyspace::key_value` that drivers return this error for a missing key, and makes the fake return it.
- There is no automatic refresh.

- [ ] **Step 1: Write the failing tests:**
  - `scan_pages_with_load_more`: a mocked `keysScan` returns a cursor and then `null`. Expect 2 calls, the button gone, and the count text.
  - `pattern_rescans_from_start`
  - `hash_value_renders_and_edits`: an inline edit calls `keyEditPreview` with `{hash_set:{key, field, value}}`, all base64.
  - `keys_with_same_display_stay_distinct`: two keys with the same `display` and different `raw`; selecting the second and deleting sends the second's `raw`.
  - `vanished_key_shows_notice`
  - Rust `fake_missing_key_is_key_not_found`: `key_value` on a missing key gives `DbError::KeyNotFound`, and its AppError kind is `key_not_found`.
  - `read_only_hides_edit_controls`
  - `hex_mode_rejects_bad_hex`
  - `keys_tab_restores_after_reload` (tab persistence)
- [ ] **Step 2: Run** them. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Write and run the e2e spec** `key-browser.spec.ts`:
  - open `db0` from the tree; the list shows 5 keys;
  - filter `user:*` leaves 1;
  - open the hash, edit a field, confirm, and the new value shows;
  - set a TTL, then delete the key on a Production-tagged connection, which asks for confirmation.

  Expected: PASS.
- [ ] **Step 5: Commit** `feat(ui): key browser tab`.

### Task 9: Full verification and hand-off

- [ ] **Step 1: Run the CI set:** `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `pnpm --dir ui lint`, `pnpm --dir ui test`, and `$env:E2E_WEB_PORT='5176'; pnpm --dir e2e-web test`. Expected: PASS, with no `.snap.new` files.
- [ ] **Step 2: Check the ESLint rule holds:** `pnpm --dir ui exec eslint src --rule '{}' --format compact | Select-String no-restricted-syntax` prints nothing.
- [ ] **Step 3: Visual check** in e2e-web with screenshots of the dialog (Mongo and Redis), the Redis tree and key browser, and the Mongo JSON view, in light and dark. Save them under `e2e-web/r4b-shots/` (not committed).
- [ ] **Step 4: Write** `docs/superpowers/plans/2026-11-04-r4b-followups.md`. It records decisions carried to the driver specs (decisions 1, 4, 6 and 7) and anything found in review. Commit `docs: R4b hand-off`.
