# MongoDB tooling: schema, document editor, indexes, export/import, explain, keepalive, bulkWrite

- Date: 2026-11-09
- Status: written from the suggestions the user approved ("follow the suggestions, no Redis yet"). Awaiting the
  user's review.
- Basis:
  - the MongoDB driver spec `2026-11-05-mongo-driver-design.md` and its hand-off `2026-11-05-mongo-driver-followups.md`;
  - the core non-SQL spec `2026-10-31-non-sql-driver-core-design.md`;
  - the existing SQL implementations of export/import, explain and object actions.
- Out of scope here (done separately in this branch): reconnect after idle and the console formatter.
- Out of scope entirely: Redis, Explain for Redis, sharded-cluster specific views.

## 1. Goals

Seven features that take MongoDB connections from "query and edit cells" to day-to-day tooling. Each one reuses an
existing SQL path wherever the shapes match, and adds a Command-family branch where they don't. The safety promises are
unchanged:

- every write goes through a guarded plan;
- read-only connections never write;
- Production asks for confirmation;
- nothing is retried automatically;
- secrets never reach History or logs.

| # | Feature | Where the user meets it |
|---|---|---|
| C1 | Schema analysis | New `Schema` sub-tab of a collection |
| C2 | Edit / add a whole document as JSON | Data view: row menu `Edit document…`, toolbar `Add document…` |
| C3 | Index management | New `Indexes` sub-tab of a collection |
| C4 | Export and import | Result and data-view export; tree menu `Import data…` on a collection |
| C5 | Explain panel | Query tab Explain button / Ctrl+Shift+E for `find` and `aggregate` |
| C6 | Keepalive for every engine | Background; a setting; the status bar shows reconnecting |
| C7 | `bulkWrite` in the console | Console grammar |

## 2. C1 — Schema analysis

**Backend.** New command `collection_schema(id, obj, sample_size)`, available only to the Command family, gated on the
connection's kind being Mongo.

- **Sample.** `[{$sample: {size: N}}]`, with `N` one of 100, 1000 (default) or 5000. This is a random sample, unlike the
  fixed sample `table_detail` uses for fingerprints, which stays as it is.
- **Walk.** Every document is walked recursively to depth 4. Dotted paths are used for sub-documents (`address.city`),
  and array element paths use `[]` (`items[].sku`). The walk stops at depth 4, and at 2,000 distinct paths, with a
  `truncated` flag.
- **Result.** `CollectionSchema { sampled: u32, paths: Vec<FieldStat>, truncated: bool }`, where:
  - `FieldStat { path, depth, present: u32, types: Vec<(String, u32)> }`;
  - type names follow §7 of the driver spec (`objectId`, `string`, `int`, `long`, `double`, `decimal`, `bool`, `date`,
    `object`, `array`, `binData`, `uuid`, `timestamp`, `regex`, `null`, …);
  - `types` is sorted by count, then name;
  - `present` counts documents where the path exists, including when its value is `null`.
- **Execution.** It runs as a Read through the data session, so it can be cancelled and honours `maxTimeMS`. Read-only
  connections may use it.

**UI.** `Collection` sub-tabs become `data`, `schema`, `ddl`, `indexes`.

- **Header.** "Sample of N documents", a sample-size select, and a `Refresh` button.
- **Table.** A tree table with these columns:
  - field path (indented by depth, collapsible);
  - presence (a percentage plus a thin bar);
  - types (chips, each with its percentage). A field with more than one type gets a `mixed` badge.
- **Filter and copy.** A text filter on the path, as in the Info panel's columns section, and `Copy path` in the row
  menu.
- **Empty collection.** Shows "This collection has no documents."

## 3. C2 — Edit or add a whole document as JSON

**Core.** `RowEdit` gains `Replace { row: RowRef, doc: Value }`, where `doc` is a `Value::Json` holding canonical
Extended JSON. SQL `build_edits` refuse it with `Replacing a whole row is not available for this engine`.

**Mongo `build_edits`:**

- `Replace` becomes `replaceOne({_id}, doc)`.
  - `doc` is parsed with the shell literal parser (Extended JSON v2).
  - It must be a document whose `_id` is either absent or equal to the row's `_id`; when absent, the row's `_id` is
    kept. Otherwise the error is `_id cannot be changed`.
  - Top-level keys starting with `$` are refused: `A replacement document cannot contain update operators`.
- `matchedCount = 0` gives `DocumentNotFound`, as with updates. It goes into atomic apply exactly like the other edits.
- Insert from JSON reuses `RowEdit::Insert` with one `Value` cell per top-level field, or, more simply, a new
  `Insert`-from-document form. The decision: add `RowEdit::InsertDocument { doc: Value }` → `insertOne(doc)`, so nested
  documents keep their shape without being flattened into cells. SQL refuses it.

**Fetching the full document.** Page cells may be cut at 64 KiB, and nested values only travel as JSON text. Before the
dialog opens, the UI asks the backend for the full document with a new `document_fetch(id, obj, row: RowRef)`.

- It is a Read `findOne({_id})` through the data session.
- It returns `Option<Value::Json>` in canonical Extended JSON.
- A document that no longer exists gives `Document <_id> no longer exists`.

**UI.**

- **Entry points:** the data view's row context menu `Edit document…`, the `Enter` key on a row number, and the toolbar
  `Add document…`.
- **Dialog:**
  - a large Monaco JSON editor, using the `mongo` language so the console's literal syntax and highlighting apply;
  - `Format` (the console formatter);
  - a validity line ("Valid document" / the parse error with its line and column);
  - `Cancel` and `Stage` buttons.
- **Staging.** `Stage` adds the edit to the staged changes, so it goes through the usual preview, guard and apply. The
  edited row shows as modified.
- **Disabled states.** Read-only connections and Production-without-transactions follow the existing data-view edit
  rules, so no new UI checks are needed.

## 4. C3 — Index management

**Core.**

- New `IndexEdit` enum:
  - `Create { keys: Vec<(String, IndexKey)>, options: IndexOptions }`, where `IndexKey` is `Asc`, `Desc`, `Text`,
    `Hashed` or `Geo2dsphere`;
  - `IndexOptions { name: Option<String>, unique: bool, sparse: bool, hidden: bool, expire_after_seconds: Option<i64>,
    partial_filter: Option<Value /* JSON */> }`;
  - `Drop { name }`.
- `DataAccess::build_index_edit(obj, &IndexEdit) -> Result<Vec<ParsedCommand>, DbError>`, defaulting to `Unsupported`.

**Mongo `build_index_edit`:**

- `Create` → `db.<coll>.createIndex(keys, options)`.
- `Drop` → `db.<coll>.dropIndex("<name>")`. Dropping `_id_` is refused with `The _id index cannot be dropped`.
- Both are class `Ddl` with targets `[collection]`, so Production confirms by the collection name.

**Backend.** `index_edit_preview(id, obj, edit) -> EditPreview` reuses the Command edit-plan store and verdict path in
`edit_plans.rs`. It is applied with the existing `data_apply`. A single command is never run as a transaction.

**UI.** The `Indexes` sub-tab lists the indexes, from `table_detail`'s `IndexDetail`.

- **Columns:** name, keys (with direction or type), unique, sparse, TTL, partial, hidden, and size when known.
- **Actions:**
  - `Create index…` opens a dialog: a key list (field picker fed by the C1 schema paths or the sampled columns, plus
    direction or type), name, unique, sparse, hidden, a TTL in seconds, and a partial filter in JSON.
  - `Drop` in the row menu.
- Both open the existing preview dialog, which shows the exact `createIndex` / `dropIndex` text, the verdict and any
  confirmation. Read-only connections show the list without actions.
- After an apply the tab refreshes, and so does the Info panel.

## 5. C4 — Export and import

**Export:**

- `export_rows`, `export_query_result` and `export_table_impl` stop calling `require_sql()` and take
  `Option<SqlKind>`.
- `ExportFormat::Sql` needs `Some` and is refused for Mongo with `SQL INSERT export is not available for MongoDB`.
- CSV, JSON, XLSX and Markdown work for Mongo. Nested values are written as relaxed Extended JSON text in CSV, XLSX and
  Markdown.
- JSON export for Mongo writes an array of documents rebuilt from the row columns, in the same way as the JSON view, in
  canonical Extended JSON. A new `JSON lines` option writes one document per line.
- Exporting a whole collection (`all.kind == 'table'`) runs the Command data path in pages of 1,000 inside a cancellable
  export job, as SQL does. The data view's current filter and sort are applied.
- The UI hides `SQL INSERT` for Mongo, and re-enables `Export…` on Mongo data views and query results (it was hidden in
  R4b).

**Import:**

- Capabilities: Mongo `import = true`.
- The wizard's target step gains a Mongo target: an existing collection, or a new collection by name.
- Sources:
  - **CSV and XLSX:** column mapping to field names, with type inference per column (int, double, bool, ISO date, else
    string) that can be overridden per column to `string`, `int`, `double`, `bool`, `date` or `objectId`.
  - **JSON:** an array of documents or JSON lines. Documents are kept as they are, and Extended JSON wrappers are
    understood.
- **Dry run:** parses every record and reports the first errors, with nothing written.
- **Run:**
  - `insertMany` batches of 1,000, with `ordered: false`, inside an import job that has progress and Cancel.
  - The guard is class `Write`: read-only connections are refused, and Production confirms by the collection name.
  - Duplicate-key and validation errors are counted, and the first 100 are listed in the job detail.
  - No retry. A cancelled job reports how many documents were inserted.

## 6. C5 — Explain panel for Mongo

- **Capability.** Mongo `explain = true`. `query_explain_impl` gets a Command branch for Mongo.
- **Explaining the statement at the cursor:**
  - it must be a `find` (with its modifiers), `findOne`, `countDocuments`, `distinct` or `aggregate`; anything else
    gives `Explain is available for find, count, distinct and aggregate`;
  - verbosity is `executionStats`, or `queryPlanner` on read-only connections, per the driver rule;
  - the result runs through the existing Read execution path.
- **Parser.** A new `ExplainFormat::MongoJson` and `parse_mongo_plan` in `crates/query/src/explain.rs`.
  - Fields map to `PlanNode` like this:

    | Mongo field | `PlanNode` field |
    |---|---|
    | `stage` | `label` |
    | `indexName`, `keyPattern`, `direction`, `filter` (short form) | `detail` |
    | `inputStage` / `inputStages` | children |
    | `executionStats` / `executionStages` `nReturned` | `actual_rows` |
    | `executionTimeMillisEstimate` | `actual_ms` |
    | `docsExamined` / `keysExamined` | `detail` |

  - For an aggregation, each `$cursor` and the stages after it become a chain.
  - The full reply is `raw`. Unknown shapes, such as sharded plans, fall back to showing only the JSON and Raw views.
- **Warnings:**
  - `COLLSCAN` on a collection with more than 1,000 documents examined;
  - `SORT` without an index (in-memory sort);
  - `docsExamined / nReturned > 10`.

  They use `planWarnings` with Mongo rules.
- **UI.** The existing ExplainView (tree, grid, JSON and Raw views; no classic view for Mongo). The `Explain` button
  and shortcut appear for Mongo tabs.

## 7. C6 — Keepalive for every engine

- **Setting.** `Keep connections alive` in Settings → Connections, with an interval of 60 s by default (0 turns it off).
  It is stored in app settings.
- **Ping loop.** `ConnectionRuntime` gets a ping task, started when the connection opens and aborted when it closes. Each
  interval it:
  - pings the metadata session through `with_session`, which already reopens on `ConnectionLost`;
  - pings pooled data sessions that have been idle longer than the interval, dropping any whose ping fails, so the pool
    reopens them.
- **Query tab sessions.** These are pinged when idle longer than the interval and not inside a transaction. A failed ping
  marks the tab session for reopen, the same as after a lost run. Sessions with an open transaction are never pinged
  (the drivers would report the loss on use anyway), and neither are sessions with custom state.
- **Status.** A ping failure that leads to a reopen sets `ConnState::Reconnecting`, then `Connected` or `Failed`.
  - A new event, `connection_state_changed`, lets the status bar show `Reconnecting…` and `Connection lost` with a
    `Reconnect` button.
  - The tree marks a failed connection.
- **Limits.** One ping in flight per connection. Pings never run while a command is running on that session. A ping
  uses a 5 s timeout.

## 8. C7 — `bulkWrite` in the console

- **Grammar.** `db.<coll>.bulkWrite([ops], {ordered?})`. Each op is one of `insertOne: {document}`,
  `updateOne`/`updateMany: {filter, update, upsert?}`, `replaceOne: {filter, replacement, upsert?}` or
  `deleteOne`/`deleteMany: {filter}`.
- **Classification.** `Write`, fail-closed.
  - It is `unbounded` if any `updateMany` or `deleteMany` has an open filter. It reuses `any_unbounded`, which treats an
    unreadable op as unbounded.
  - An op of an unknown kind is a parse error: `Unknown bulkWrite operation <name>`.
- **Execution.** Ops are grouped into consecutive runs of the same kind and sent as `insert` / `update` / `delete`
  commands. This works on MongoDB 6.0 and later, without relying on the server's 8.0 `bulkWrite` command.
  - `ordered` (default true) stops at the first error; `ordered: false` continues and collects errors.
  - `affected` is the sum of inserted, matched and deleted counts. The `result` row shows the totals and `upsertedIds`.
- **Read-only.** The allowlist refuses it.
- **Redaction and round-trip.** It is redacted like other commands, and `coll_text` round-trips it.

## 9. Testing

- **Unit:**
  - the schema walker (depths, arrays, mixed types, truncation);
  - `Replace` and `InsertDocument` build rules and refusals;
  - `build_index_edit` and the `_id_` refusal;
  - `parse_mongo_plan` on recorded explain outputs from MongoDB 6.0 and 8.0 (find with index, COLLSCAN, sort, aggregate
    with `$lookup`), plus the warnings;
  - `bulkWrite` grammar, classification (incl. unbounded and unknown op) and grouping;
  - CSV type inference;
  - export of nested values per format.
- **Integration (MongoDB 6.0 and 8.0, `DNT_TEST_DB`):**
  - `collection_schema` on the fixture;
  - replace and insert-document applies (atomic on 8.0);
  - create, drop and refused `_id_` index;
  - export of a collection to JSON and CSV round-tripped through import;
  - import duplicate-key counting and cancel;
  - explain on an indexed and an unindexed find;
  - `bulkWrite` ordered and unordered with an error;
  - keepalive (a forced drop is detected by the ping and the connection reopens).
- **App and UI:** sub-tabs, the dialogs (document editor, index create, import target), Explain button visibility, the
  status bar on reconnecting, and the e2e-web fake for each new command.
- MySQL, PostgreSQL and Redis behaviour stays unchanged. SQL snapshots stay identical.

## 10. Order of work

1. C7 `bulkWrite`, and C6 keepalive.
2. C1 Schema.
3. C2 document editor.
4. C3 indexes.
5. C5 explain.
6. C4 export/import.

C4 is the largest. C1 feeds the field pickers of C3 and C4.
