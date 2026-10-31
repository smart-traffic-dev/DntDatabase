# Data seeder Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** "Generate data…" on a table opens a dialog that fills it with realistic fake rows through a background job.

**Architecture:** A pure Rust engine (`dnt_transfer::seed`: PRNG, word lists, generators, inference, row
engine) produces `FullValue` rows; `src-tauri/src/seed_data` reads the table, previews, builds a single-use
plan and runs a `seed` job that reuses the import `Writer` / `Layout` / `BatchBuilder` / pre-step so every
statement goes through the guard. The React `SeedDialog` (Configure → Review) only collects choices and shows
what the backend answers.

**Tech Stack:** Rust (tokio, serde, specta/tauri-specta, `time`), SQLite store migrations, React 19 +
zustand + vitest, Playwright e2e-web with the fake IPC backend.

**Spec:** `docs/superpowers/specs/2026-10-30-data-seeder-design.md` (read it with this plan; § numbers below
refer to it).

## Global Constraints

- All UI copy, identifiers, log/summary/error text in English; exact strings below are copied from the spec.
- **Do not commit** anything (user instruction). Never stage or revert other sessions' uncommitted files
  (`Sidebar.tsx`, `theme.css`, `crates/core/src/sql/classify.rs`, `crates/query/src/guard.rs`,
  `src-tauri/src/commands/{mod,settings}.rs`, `ui/src/bindings.ts`, `e2e-web/src/backend.ts`, …): in shared
  files only append the seeder's lines.
- No new external crate or npm package. `time` (already a workspace dependency) may be added to
  `dnt-transfer`.
- SQL is built only through `dnt_core::sql::build` / `quote_ident` / `TableRef::relation`; the engine never
  produces SQL text.
- Every write statement passes `import_data::verdict` right before it is sent (through `Writer`).
- Limits: rows 1–1,000,000 (default 1,000); `batch_rows` 0 → 1,000, max 100,000; ≤ 1 MiB SQL per batch;
  FK sample `LIMIT 10000`; unique retries 50; preview 10 rows; key cache 60 s / 32 entries; seed is `u32`;
  `Sequence`/`IntRange` bounds within ±2^53.
- Design tokens `--dnt-*`; focus = 1px accent border; flat tabs; modal style of `e2e-web/mockups/modals-v2.html`.
- Bindings: after any command/type change run `cargo run -p dntdatabase --example export_bindings`;
  `cargo test -p dntdatabase --test commands bindings_are_current` must pass.
- e2e-web: `$env:E2E_WEB_PORT='5176'` (a stale server on 5174 serves a blank page).

## Review Focus

1. `varchar(n)` too short for a unique fake value plus its suffix (e.g. `varchar(8)` Email, 1,000 rows): the
   body is cut and the suffix kept; if even the suffix does not fit, a column issue — never silent duplicates
   (Task 4 test `unique_suffix_survives_max_chars`, `unique_suffix_too_long_is_an_issue`).
2. Identifiers with quotes / reserved words (`order`, `we"ird`, ``back`tick``) in the table, columns and FK
   parent: every statement quotes them (Task 7 test `fk_key_query_quotes_identifiers`, Task 8 test
   `job_quotes_reserved_and_quoted_names`).
3. User ranges beyond the column type (`tinyint` 0–1000, `numeric(5,2)` up to 10000, `int unsigned` min −5,
   date `2026-02-30`, `from` > `to`): a column issue at preview, never a server error mid-job (Task 2 tests).
4. Stop while a large job is generating: the generator thread ends when the receiver is dropped, `ROLLBACK`,
   no `COMMIT` (Task 8 test `stop_rolls_back_and_ends_generator`).
5. A composite FK whose parent rows have NULL in one key column, and a PG FK to another schema: NULL tuples
   are excluded, the relation is `"other"."parent"` (Task 7 test `fk_keys_skip_null_tuples_and_use_ref_schema`).

---

### Task 1: Store — `seed` job kind and history origin (MIGRATION_9)

**Files:**
- Modify: `crates/store/src/db.rs` (add `MIGRATION_9`, append to `migrations()`, migration test)
- Modify: `crates/store/src/history.rs` (`HistoryOrigin::Seed` ↔ `"seed"`)
- Modify: `crates/store/src/jobs.rs:19` (doc comment lists `seed`)

**Interfaces:**
- Produces: `HistoryOrigin::Seed` (as_str `"seed"`); table `jobs` accepts `kind = 'seed'`;
  `query_history` accepts `origin = 'seed'`.

- [ ] **Step 1: Write failing tests** in `db.rs` tests: `migration_9_accepts_seed_and_keeps_rows` — build a DB
  with migrations 1–8, insert one `jobs` row (`kind='import'`) and one `query_history` row (`origin='import'`),
  run all migrations, assert both rows (and ids) survive, and that inserting `jobs.kind='seed'` and
  `query_history.origin='seed'` succeeds while `'bogus'` still fails. In `history.rs` extend the round-trip
  table with `(HistoryOrigin::Seed, "seed")`.
- [ ] **Step 2: Run** `cargo test -p dnt-store` — expect FAIL (no `Seed`, CHECK rejects `'seed'`).
- [ ] **Step 3: Implement** `MIGRATION_9` exactly like MIGRATION_7's rebuild pattern: `jobs_next` with
  `CHECK (kind IN ('import','import_dry_run','dump','tool_dump','restore','tool_restore','export','seed'))`,
  copy, drop, rename, recreate `jobs_created`; `query_history_next` with origin CHECK
  `('query','data_edit','object_action','table_design','import','restore','seed')`, copy, drop, rename,
  recreate `query_history_conn_time`. Add `HistoryOrigin::Seed`.
- [ ] **Step 4: Run** `cargo test -p dnt-store` — expect PASS.

### Task 2: Seed engine core — PRNG, generator types, typed values and type limits

**Files:**
- Modify: `crates/transfer/Cargo.toml` (`time.workspace = true`, features `parsing`, `formatting`, `macros` as
  the workspace allows), `crates/transfer/src/lib.rs` (`pub mod seed;`)
- Create: `crates/transfer/src/seed/mod.rs`, `seed/rng.rs`, `seed/generator.rs`, `seed/limits.rs`
- Test: unit tests in each file

**Interfaces:**
- Produces (`dnt_transfer::seed`):
  - `rng::SplitMix64 { fn new(state: u64) -> Self; fn next_u64(&mut self) -> u64; fn below(&mut self, n: u64) -> u64; fn percent(&mut self, p: u8) -> bool }`
  - `rng::cell_rng(seed: u32, row: u64, column: u32) -> SplitMix64`
  - `generator::{Generator, FakeKind, ColumnPlan}` exactly as spec §4.1 (serde `tag = "kind"`, `rename_all = "snake_case"`, specta `Type`; `FakeKind` serialised snake_case).
  - `ColumnIssue { pub column: String, pub message: String }` (serde + specta).
  - `limits::int_bounds(col: &TargetColumn, kind: DbKind) -> (i128, i128)` (tinyint, smallint, mediumint, int/integer/serial, bigint/bigserial; unsigned → min 0), `limits::decimal_spec(db_type: &str) -> Option<(u32, u32)>` (precision, scale of `decimal(p,s)` / `numeric(p,s)`).
  - `generator::generate(gen: &Generator, col: &TargetColumn, rng: &mut SplitMix64, row: u64, kind: DbKind) -> Option<FullValue>` — `None` only for `Skip` and `ForeignKey` (the engine of Task 4 handles those); `Sequence` uses `start + step * row`. `Fake` and `Pattern` arms are added in Task 3 (in this task they return `Some(Text(String::new()))`, replaced there).
  - `generator::check_generator(gen: &Generator, col: &TargetColumn, kind: DbKind) -> Option<String>` (the column issue message, or `None`).

- [ ] **Step 1: Write failing tests:**
  - `cell_rng_is_deterministic_and_independent`: same `(seed,row,col)` → same first 4 `next_u64`; changing `col` changes them; changing `row` changes them.
  - `int_range_respects_bounds`: 10,000 draws of `IntRange{min:-3,max:3}` on `int` all in −3..=3 and every value seen.
  - `int_bounds_by_type`: `tinyint` → (−128,127); `tinyint unsigned` (MySQL, `unsigned: true`) → (0,255); `smallint` → (−32768,32767); `bigint` → (−2^63, 2^63−1); `integer` (PG) → (−2^31, 2^31−1).
  - `decimal_range_has_scale`: `DecimalRange{min:1.0,max:2.0,scale:2}` on `decimal(12,2)` → `Decimal` strings matching `^\d+\.\d{2}$`, value in [1,2].
  - `check_rejects_out_of_type_ranges`: `IntRange{0,1000}` on `tinyint` → `Some("Values must be between -128 and 127.")`; `IntRange{-5,10}` on unsigned int → `Some("Values must be between 0 and 4294967295.")`; `DecimalRange{0,10000,2}` on `numeric(5,2)` → `Some("Values must be below 1000.")`; `IntRange{5,1}` → `Some("The minimum is above the maximum.")`.
  - `check_rejects_bad_dates`: `DateRange{"2026-02-30","2026-03-01"}` → `Some("Enter dates as YYYY-MM-DD.")`; `from` > `to` → `Some("The start is after the end.")`.
  - `dates_in_range_and_formatted`: `DateRange` → `Text` `YYYY-MM-DD` within range; `DateTimeRange` → `YYYY-MM-DD HH:MM:SS`; `TimeRange` → `HH:MM:SS`.
  - `typed_values`: `Bool` → `Value::Bool`; `Uuid` → `Value::Uuid` v4 format; `JsonObject` → `Value::Json` parsing as an object; `RandomBytes{len:16}` → 16 bytes; `Sequence{start:10,step:5}` at rows 0, 1, 2 → `Int` 10, 15, 20; `Skip` / `ForeignKey` → `None`.
  - Serde shape: `serde_json::to_value(Generator::IntRange{min:1,max:2})` == `{"kind":"int_range","min":1,"max":2}`; `Generator::Fake{fake:FakeKind::FirstName}` == `{"kind":"fake","fake":"first_name"}`.
- [ ] **Step 2: Run** `cargo test -p dnt-transfer seed` — expect FAIL (module missing).
- [ ] **Step 3: Implement** the files. `cell_rng`: `SplitMix64::new(mix(mix(seed as u64) ^ row.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ ((column as u64) << 32)))` where `mix` is the splitmix64 finaliser. Integer values become `Value::Int`, or `Value::UInt` when above `i64::MAX`. Dates via `time::Date` (`from_julian_day` between the two bounds). Exact issue strings as in the tests.
- [ ] **Step 4: Run** `cargo test -p dnt-transfer seed` — expect PASS.

### Task 3: Fake data — word lists, `FakeKind`, `Pattern`

**Files:**
- Create: `crates/transfer/src/seed/words.rs`, `crates/transfer/src/seed/fake.rs`
- Modify: `crates/transfer/src/seed/generator.rs` (`Fake` and `Pattern` arms)

**Interfaces:**
- Produces: `fake::fake_text(kind: FakeKind, rng: &mut SplitMix64) -> String`; `fake::pattern(p: &str, rng: &mut SplitMix64) -> String`; `fake::with_suffix(kind: Option<FakeKind>, value: &str, n: u64) -> String` (Email `local.n@domain`, Username `{value}{n}`, else `{value}-{n}`).

- [ ] **Step 1: Write failing tests:**
  - `emails_look_real`: 1,000 emails match `^[a-z]+\.[a-z]+(\.\d+)?@[a-z]+\.(com|net|org|io)$`.
  - `phones_ipv4_colors_slugs`: Phone matches `^\(\d{3}\) \d{3}-\d{4}$`; Ipv4 four octets 1–254; HexColor `^#[0-9a-f]{6}$`; Slug `^[a-z]+(-[a-z]+){1,3}$`; ZipCode `^\d{5}$`.
  - `names_from_lists`: FirstName ∈ `words::FIRST_NAMES`; FullName is `"{first} {last}"` from the lists.
  - `sentences_and_paragraphs`: Sentence starts uppercase, ends with `.`, 4–12 words; Paragraph 2–5 sentences.
  - `password_hash_shape`: `^\$2b\$10\$[./A-Za-z0-9]{53}$`.
  - `pattern_tokens`: `pattern("SKU-???-#####")` matches `^SKU-[A-Z]{3}-\d{5}$`; `"*"` ∈ `[A-Z0-9]`; `"\\#x"` == `"#x"`.
  - `with_suffix_cases`: Email `john.smith@example.com`,7 → `john.smith.7@example.com`; Username `jsmith`,7 → `jsmith7`; City `Austin`,7 → `Austin-7`.
- [ ] **Step 2: Run** `cargo test -p dnt-transfer seed::fake` — expect FAIL.
- [ ] **Step 3: Implement** word lists (≥ 100 first names, ≥ 100 last names, ≥ 50 cities with states, ≥ 40 countries, ≥ 50 job titles, ≥ 200 lorem words, company words + suffixes `Inc`, `LLC`, `Group`, `Ltd`, domains `example`, `mail`, `test`, … with TLDs `com`, `net`, `org`, `io`) and the generators; wire `Fake` / `Pattern` into `generate`.
- [ ] **Step 4: Run** `cargo test -p dnt-transfer seed` — expect PASS.

### Task 4: Row engine — `Seeder`, NULL %, unique, `max_chars`, FK keys, `check_plans`

**Files:**
- Create: `crates/transfer/src/seed/engine.rs`
- Test: `crates/transfer/tests/seed.rs` (integration + proptest)

**Interfaces:**
- Consumes: Tasks 2–3.
- Produces:
  - `pub struct FkKeys { pub constraint: String, pub columns: Vec<String>, pub keys: Arc<Vec<Vec<FullValue>>> }`
  - `pub struct SeedColumnSpec { pub column: TargetColumn, pub plan: ColumnPlan, pub fk: Option<Arc<FkKeys>> }`
  - `pub fn check_plans(specs: &[SeedColumnSpec], rows: u32, kind: DbKind) -> Vec<ColumnIssue>`
  - `pub struct Seeder` with `Seeder::new(specs: Vec<SeedColumnSpec>, seed: u32, kind: DbKind) -> Result<Seeder, Vec<ColumnIssue>>` (runs `check_plans` with `rows = 1`), `fn columns(&self) -> Vec<TargetColumn>` (non-`Skip` columns, table order), `fn row(&mut self, row: u64) -> Result<Vec<FullValue>, ColumnIssue>` (rows must be requested in order 0, 1, 2, …).

- [ ] **Step 1: Write failing tests** (`tests/seed.rs`):
  - `same_seed_same_rows` / `other_column_unaffected`: two seeders, same specs → identical first 100 rows; change column B's generator → column A identical.
  - `null_percent`: nullable column, `null_percent: 30`, 10,000 rows → NULL share in 0.27–0.33; NOT NULL column with `null_percent: 50` → no NULL.
  - `skip_columns_are_left_out`: `columns()` excludes `Skip`; row length equals `columns().len()`.
  - `sequence_uses_row_index`: rows 0..3 of `Sequence{start:1,step:1}` → 1,2,3.
  - `unique_fake_gets_suffix`: FirstName + `unique: true`, 5,000 rows → all distinct.
  - `unique_suffix_survives_max_chars`: `varchar(12)` Email unique 1,000 rows → all distinct, all ≤ 12 chars, duplicates end with the row number.
  - `unique_suffix_too_long_is_an_issue`: `varchar(3)` Username unique 5,000 rows → `row()` eventually returns `ColumnIssue{message: "Cannot generate 5000 unique values for `u`; widen the range."}` or `check_plans` reports it — assert one of them, never duplicates.
  - `check_plans_unique_feasibility`: `IntRange{1,100}` unique, rows 1,000 → issue `Cannot generate 1000 unique values for `n`; widen the range.`; `Pick{3 values}` unique rows 4 → same message shape; `Bool` unique rows 3 → issue.
  - `check_plans_rules`: generated column not `Skip` → `Generated column: the server computes it.`; NOT NULL no default `Skip` → `{col} has no default; choose a generator.`; `Pick` value not in `enum_values` → `"x" is not one of the enum values.`; `Constant{"abc"}` on `int` → issue from `dnt_transfer::convert` text; `ForeignKey` with `fk: None` → `The foreign key has no keys to pick from.`.
  - `fk_tuples_stay_together`: FK over `(a,b)` keys `[(1,10),(2,20)]` → every row has a matching pair; nullable FK with `null_percent: 100` → both NULL.
  - `fk_empty_parent`: keys empty, NOT NULL → `check_plans` issue `{parent} has no rows to reference.`; nullable → no issue, values NULL.
  - proptest `values_within_type_limits`: random `IntRange`/`DecimalRange` inside `int_bounds`/`decimal_spec` and random `max_chars` 1–50 with any `FakeKind` → every value within bounds and ≤ `max_chars` chars.
- [ ] **Step 2: Run** `cargo test -p dnt-transfer --test seed` — expect FAIL.
- [ ] **Step 3: Implement** `Seeder`: per cell `cell_rng(seed,row,col)`; NULL draw first; unique via `HashSet<String>` of a canonical text of the value; `Fake` text kinds suffix with `row + 1` on collision, others retry up to 50 times on the same rng; `max_chars` cuts the body before the suffix (char-boundary safe); FK picks `keys[rng.below(len)]` with the rng of the FK's first column, shared by its columns (cache the tuple per row).
- [ ] **Step 4: Run** `cargo test -p dnt-transfer` — expect PASS (all old tests too).

### Task 5: Inference — generator from name/type, CHECK `IN` parsing

**Files:**
- Create: `crates/transfer/src/seed/infer.rs`

**Interfaces:**
- Produces:
  - `pub fn infer_plan(col: &TargetColumn, detail: &TableDetail, today: time::Date) -> ColumnPlan`
  - `pub fn check_values(detail: &TableDetail, column: &str) -> Vec<String>`
  - `pub fn unique_single(detail: &TableDetail, column: &str) -> bool` (PK or unique, non-partial, one-column index)
  - `pub fn fk_of<'a>(detail: &'a TableDetail, column: &str) -> Option<&'a ForeignKeyDetail>`

- [ ] **Step 1: Write failing tests:** a table-driven `infers_by_name_and_type` covering every row of spec §4.3
  table (e.g. `("email","varchar(120)",Text)` → `Fake Email`; `("customerName","varchar(80)",Text)` → `Fake FullName`; `("price","decimal(10,2)",Decimal)` → `DecimalRange{1,1000,2}`; `("is_active","tinyint(1)",Integer)` → `Bool{50}`; `("code","varchar(20)",Text)` → `Pattern("???-#####")`; `("dob","date",Date)` → `DateRange{"1950-01-01","2005-12-31"}`; `("created_at","timestamp",DateTime)` with today 2026-10-30 → `DateTimeRange{"2024-10-30 00:00:00","2026-10-30 23:59:59"}`; `("title","varchar(255)")` → Sentence; `("notes","text")` → Paragraph; `("notes","varchar(100)")` → Sentence; `("x","varchar(8)")` → `Pattern("????????")`; `("x","int")` → `IntRange{1,1000}`; `("x","tinyint")` → `IntRange{1,127}`; `("x","jsonb",Json)` → JsonObject; `("x","bytea",Binary)` → `RandomBytes{16}`; `("x","int[]",Array)` nullable → `Constant{""}` + `null_percent 100`; with default → `Skip`).
  - Precedence: auto_increment → Skip; identity → Skip; generated → Skip; FK column named `email` → `ForeignKey`; enum column named `name` → `Pick`; `uuid` type → `Uuid`.
  - `unique_default`: PK and single-column unique index → `unique: true`; composite unique / partial → false.
  - `parses_check_in_lists`: MySQL `(`status` in (_utf8mb4'new',_utf8mb4'paid'))` → `["new","paid"]`; PG `((status)::text = ANY ((ARRAY['new'::character varying, 'paid'::character varying])::text[]))` → `["new","paid"]`; `CHECK (status IN ('a','b'))` → `["a","b"]`; `total >= 0` → `[]`; a check on two columns → `[]`.
- [ ] **Step 2: Run** `cargo test -p dnt-transfer seed::infer` — expect FAIL.
- [ ] **Step 3: Implement** token split (snake + camel, lowercase), rules in spec order, a small hand-written
  CHECK scanner (quoted literals with `''` escapes, optional `_charset` introducers and `::type` casts).
- [ ] **Step 4: Run** `cargo test -p dnt-transfer` — expect PASS.

### Task 6: src-tauri groundwork — shared fake server, generic `Writer`

**Files:**
- Create: `src-tauri/src/fake_server.rs` (`#[cfg(test)] mod fake_server;` in `src-tauri/src/lib.rs` or the crate root that declares `import_data`)
- Modify: `src-tauri/src/import_data/tests.rs` (use `crate::fake_server::*`; delete the moved code)
- Modify: `src-tauri/src/import_data/run.rs`, `src-tauri/src/import_data/mod.rs`

**Interfaces:**
- Produces:
  - `fake_server::{Fake, FakeDriver, fixture_with_detail(kind, detail: TableDetail, env, read_only, plan_ttl) -> Fx, items_detail, column, index}`; `Fake::rows_for(contains: &str, cols: Vec<ColumnMeta>, rows: Vec<Vec<Value>>)` — a statement containing `contains` streams those rows to the sink.
  - `run::Msg` with `pub(crate)` fields and `pub(crate) fn Msg::row(record: u64, values: Vec<FullValue>, done: u64, total: u64) -> Msg` / `Msg::end(done: u64) -> Msg` (End with no warnings, `sha256: None`).
  - `Writer::new(…, phase, unit: ProgressUnit)` `pub(crate)`; `Writer::pump`, `send`, `send_commit`, `cleanup`, fields `in_tx`, `imported`, `written` `pub(crate)`.
  - `pub(crate) enum PreStepState` and `pub(crate) async fn describe_failure(w: &Writer<'_>, e: DbError, pre: PreStepState, statement_atomic: bool, words: &FailureWords) -> (JobFailure, Option<u64>)` with `pub(crate) struct FailureWords { pub rolled_back: &'static str, pub refused_commit: &'static str }`; `ImportJob::failure` calls it with `"The import was rolled back; no row was imported."` / `"The server refused the COMMIT; no row was imported."`.

- [ ] **Step 1: Move** the fake server and fixtures; add `rows_for`. Add test `fake_streams_programmed_rows` in `fake_server.rs`.
- [ ] **Step 2: Refactor** `Writer` (unit field used by `progress`), `Msg`, `describe_failure`. Import passes `ProgressUnit::Bytes`.
- [ ] **Step 3: Run** `cargo test -p dntdatabase import_data` and `cargo test -p dntdatabase jobs` — expect PASS with no test text changed (byte-identical messages).

### Task 7: `seed_data` module — types, `normalise`, `seed_target`, FK key reading and cache

**Files:**
- Create: `src-tauri/src/seed_data/mod.rs`, `seed_data/target.rs`, `seed_data/keys.rs`, `seed_data/tests.rs`
- Modify: `src-tauri/src/jobs/model.rs` (`JobKind::Seed` `"seed"` in `ALL` (8 items), `JobParams::Seed(serde_json::Value)`), `src-tauri/src/session_manager.rs` (fields `seed_plans: crate::seed_data::SeedPlanStore`, `seed_keys: crate::seed_data::SeedKeyCache`; `drop_conn` next to `import_plans.drop_conn` at line ~810), crate root (`mod seed_data;`)

**Interfaces:**
- Consumes: Tasks 4–6; `import_data::{table_ref, session_db, target::read_target_bounded, ConfirmLevel, ImportConfirm, PreStep}`.
- Produces: the IPC types of spec §5.1 (`ExistingRows`, `SeedRequest`, `SeedForeignKey`, `SeedColumn`, `SeedTarget`, `SeedSample`, `SeedPreview`; `fk_keys` is `Vec<SeedKeyCount { constraint: String, count: u32 }>`), plus
  - `pub(crate) fn normalise(req: SeedRequest, kind: DbKind) -> Result<SeedRequest, DbError>` (§5.2 table/rows/batch checks; config errors with field `table` / `rows`).
  - `pub(crate) fn specs(req: &SeedRequest, read: &TargetRead, keys: &HashMap<String, Arc<FkKeys>>) -> Result<Vec<SeedColumnSpec>, DbError>` (column set mismatch → `PlanStale { reason: "The table changed; check the generators and preview again." }`).
  - `keys::fk_key_sql(kind: DbKind, database: &str, fk: &ForeignKeyDetail) -> String` (`SELECT DISTINCT … WHERE … IS NOT NULL [AND …] ORDER BY … LIMIT 10000`; parent relation from `ref_schema` / table database).
  - `keys::read_fk_keys(session: &dyn Session, fk: &ForeignKeyDetail, database: &str) -> Result<Vec<Vec<FullValue>>, DbError>` (bounded by `METADATA_CALL_TIMEOUT`).
  - `SeedKeyCache::get_or_read(&self, conn: Uuid, generation: u64, fk: …, read: impl Future) -> Result<Arc<FkKeys>, DbError>` (60 s, 32 entries).
  - `SessionManager::seed_target(&self, conn: Uuid, table: ObjectRef) -> Result<SeedTarget, DbError>` (`seed` from `rand`-free source: low 32 bits of `Uuid::new_v4()`; `today` from `time::OffsetDateTime::now_utc().date()`).

- [ ] **Step 1: Write failing tests** (`seed_data/tests.rs`, fixture `orders(id int AI PK, customer_id int NOT NULL FK→customers(id), email varchar(120) NOT NULL UNIQUE, status enum('new','paid','shipped'), total decimal(12,2), note text NULL)`):
  - `target_infers_generators`: `seed_target` → id `Skip` locked=false, customer_id `ForeignKey{constraint}` with `foreign_key.ref_table == "customers"`, email `Fake Email` unique, status `Pick[new,paid,shipped]`, total `DecimalRange{1,1000,2}`, note Paragraph; `read_only`/`env` copied.
  - `normalise_rules`: rows 0 and 1,000,001 → config error field `rows` with `Choose between 1 and 1,000,000 rows.`; MySQL schema → `MySQL has no schemas inside a database.`; batch 0 → 1,000.
  - `fk_key_query_quotes_identifiers`: parent `` we`ird `` on MySQL → ``SELECT DISTINCT `id` FROM `shop`.`we``ird` WHERE `id` IS NOT NULL ORDER BY `id` LIMIT 10000``.
  - `fk_keys_skip_null_tuples_and_use_ref_schema`: PG composite FK `(a,b)` → `ref_schema "other"` → `… FROM "other"."parent" WHERE "a" IS NOT NULL AND "b" IS NOT NULL ORDER BY "a", "b" LIMIT 10000`; rows programmed with `rows_for` come back as `FullValue`s.
  - `key_cache_reuses_within_ttl`: two `get_or_read` within TTL → one query sent (count via `Fake::since`).
- [ ] **Step 2: Run** `cargo test -p dntdatabase seed_data` — expect FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p dntdatabase seed_data` and `cargo test -p dntdatabase jobs` — expect PASS.

### Task 8: Preview, prepare, start and the seed job

**Files:**
- Create: `src-tauri/src/seed_data/plan.rs`, `src-tauri/src/seed_data/run.rs`
- Test: `src-tauri/src/seed_data/tests.rs`

**Interfaces:**
- Consumes: Tasks 6–7.
- Produces:
  - `SessionManager::seed_preview(&self, req: SeedRequest) -> Result<SeedSample, DbError>` (issues returned in `issues`, never `Err`, for column problems; ≤ 10 rows = `min(rows, 10)`; `sql` = those rows built by `Layout` builder, then `dnt_query::redact::redact` for display).
  - `SessionManager::seed_prepare(&self, req: SeedRequest) -> Result<SeedPreview, DbError>` (read-only → `Blocked{count:1}`; any issue → `config_error("columns", "<first issue column>: <message>")`; pre-step via import's `pre_step` mapping `Delete`→`Replace{truncate:false}`, `Truncate`→`Replace{truncate:true}`; confirm per spec §5.5 / D8; warnings per §5.5 with exact texts: `Not transactional: batches already written stay if the job stops.`, `{parent} has no rows: {col} is always NULL.`, `Keys of referenced tables are read again when the job starts; rows may differ from the preview.`, `The table has a unique key over several columns: duplicates fail the job.`, plus import's TRUNCATE / trigger / irreversible texts).
  - `SessionManager::seed_start(&self, plan_id: Uuid, confirm: ImportConfirm) -> Result<Uuid, DbError>` (as `import_start`; `JobSpec { kind: JobKind::Seed, title: format!("Generate data for {table}"), params: JobParams::Seed(request), rerunnable: true }`).
  - `run::SeedJob: JobRunner` — sequences of spec §5.4; generator thread (`spawn_blocking`) sends `Msg::row` per row (progress phase `"Generating"`, unit `Rows`) and stops when the receiver is dropped; success summary `Inserted {n} rows into {table} in {b} batches` (`1 batch` when `b == 1`, via `run::plural`); failure via `describe_failure` with `"The job was rolled back; no row was inserted."` / `"The server refused the COMMIT; no row was inserted."`; one History entry `HistoryOrigin::Seed`, text `-- Generate {n} rows into {table}: seed {seed}, {keep|delete first|truncate first}\n` + pre-step + `head…tail`, redacted.

- [ ] **Step 1: Write failing tests:**
  - `pg_job_statements`: rows 3, existing Keep → sent (after reads) `SET standard_conforming_strings = on`, `BEGIN`, FK key `SELECT DISTINCT …`, one `INSERT INTO "public"."orders" ("customer_id", "email", "status", "total", "note") VALUES (…), (…), (…)`, `COMMIT`; job Done with summary `Inserted 3 rows into orders in 1 batch`.
  - `mysql_truncate_is_irreversible_first`: InnoDB, Truncate → `TRUNCATE` before `START TRANSACTION`; prepare's `confirm` is `TypeName{name:"orders"}` on a connection without env; `pre_step.irreversible == true`; start without `irreversible_ack` → `ConfirmationRequired`.
  - `mysql_myisam_autocommit`: no `START TRANSACTION`, warning `Not transactional: …`.
  - `read_only_blocked`: prepare → `Blocked`; connection switched to read-only after start (before the job runs) → job Failed `blocked`, no INSERT sent.
  - `production_needs_typed_name`: Production, Keep → `TypeName`; wrong name → `ConfirmationRequired`; Staging → `Click`.
  - `stop_rolls_back_and_ends_generator`: rows 200,000, batch 100, delay on `INSERT` → cancel after first INSERT → sent ends with `ROLLBACK`, no `COMMIT`, state Cancelled; the generator thread has exited (a `Weak`/counter exposed by the test hook drops to 0 within 1 s).
  - `table_changed_between_prepare_and_start` → `PlanStale`.
  - `preview_equals_first_rows`: `seed_preview` rows (as display values) equal the first 10 tuples of the job's first INSERT (compare the preview `sql` tuples with the sent INSERT's first 10 tuples).
  - `empty_parent_not_null_is_an_issue`: FK keys empty → preview issue `customers has no rows to reference.` on `customer_id`; prepare → config error.
  - `job_quotes_reserved_and_quoted_names`: table `order`, column `we"ird` (PG) → INSERT head `INSERT INTO "public"."order" ("we""ird") VALUES `.
  - `history_entry_has_no_values`: History row origin `seed`, text starts `-- Generate 3 rows into orders: seed 7, keep`, contains no generated email.
- [ ] **Step 2: Run** `cargo test -p dntdatabase seed_data` — expect FAIL.
- [ ] **Step 3: Implement.** Plan store mirrors `ImportPlanStore` (max 64, `plan_ttl`, `drop_conn`); the job re-reads `table_detail`, compares the fingerprint, judges the templates, reads FK keys on its own session (no cache) before the first INSERT.
- [ ] **Step 4: Run** `cargo test -p dntdatabase` — expect PASS.

### Task 9: Tauri commands and bindings

**Files:**
- Create: `src-tauri/src/commands/seed.rs`
- Modify: `src-tauri/src/commands/mod.rs` (`pub mod seed;`, register after `import_data::import_start`: `seed::seed_target`, `seed::seed_preview`, `seed::seed_prepare`, `seed::seed_start`)
- Modify (generated): `ui/src/bindings.ts`, `ui/src/test/fixtures` only if a fixture enumerates `JobKind`

**Interfaces:**
- Produces TS commands `seedTarget(connectionId, table)`, `seedPreview(request)`, `seedPrepare(request)`, `seedStart(planId, confirm)` and types `SeedRequest`, `SeedTarget`, `SeedColumn`, `SeedForeignKey`, `SeedSample`, `SeedKeyCount`, `SeedPreview`, `ColumnPlan`, `Generator`, `FakeKind`, `ColumnIssue`, `ExistingRows`; `JobKind` gains `"seed"`, `JobParams` gains `{ kind: "seed"; request: JsonValue }`.

- [ ] **Step 1:** Commands as thin `guard(state.sessions.seed_…)` wrappers (same doc-comment style as `import_data.rs`).
- [ ] **Step 2: Run** `cargo run -p dntdatabase --example export_bindings`, then `cargo test -p dntdatabase --test commands` — expect PASS; `pnpm --dir ui exec tsc --noEmit` — expect errors only where `Record<JobKind, …>` is now incomplete (`jobFormat.ts`), fixed in Task 11.

### Task 10: UI state — `seedState.ts`

**Files:**
- Create: `ui/src/transfer/seed/seedState.ts`, `ui/src/transfer/seed/seedFormat.ts`, `ui/src/transfer/seed/seedState.test.ts`

**Interfaces:**
- Consumes: Task 9 types.
- Produces:
  - `interface SeedState { connectionId: string; table: ObjectRef; target: SeedTarget | null; rows: number; batchRows: number; existing: ExistingRows; seed: number; plans: ColumnPlan[]; step: 'configure' | 'review'; sample: SeedSample | null; preview: SeedPreview | null }`
  - `initialState({ connectionId, table, params?: SeedRequest }): SeedState`; `reduce(state, action): SeedState` with actions `targetLoaded`, `setRows`, `setBatch`, `setExisting`, `setSeed`, `setGenerator {column, generator}`, `setPlan {column, patch: Partial<ColumnPlan>}`, `sampled {sample}`, `prepared {preview}`, `back`.
  - `buildRequest(state): SeedRequest | null`; `defaultGenerator(kind: Generator['kind'], col: TargetColumn): Generator` (defaults when switching: IntRange 1–1000 clamped, DecimalRange 0–1000 scale from type, ranges last 2 years, Pick from enum/check values, Pattern `???-#####`, Constant `""`, RandomBytes 16, Fake Word).
  - `seedFormat.ts`: `GENERATOR_LABEL: Record<Generator['kind'], string>` (`Skip (default)`, `Sequence`, `Integer range`, `Decimal range`, `Boolean`, `Date range`, `Date-time range`, `Time range`, `Pick from list`, `Foreign key`, `Fake data`, `UUID`, `Constant`, `Pattern`, `JSON object`, `Random bytes`), `FAKE_LABEL: Record<FakeKind, string>` (`First name`, …, `Password hash`), `generatorsFor(col: SeedColumn): Generator['kind'][]` (locked → `['skip']`; FK → includes `foreign_key`; by logical type).

- [ ] **Step 1: Write failing tests:** `targetLoaded` takes the inferred plans; `params` (Run again) keeps the request's plans/seed/rows over the inferred ones when the column set matches, else falls back to inferred; `setGenerator` replaces options with `defaultGenerator`; `setPlan` patches only that column; `buildRequest` returns the full column list in table order; any edit after `prepared` clears `preview` and returns to `configure`; `generatorsFor` locked → `['skip']`, integer column has no `fake`, text column has no `int_range`.
- [ ] **Step 2: Run** `pnpm --dir ui exec vitest run src/transfer/seed/seedState.test.ts` — expect FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the same — expect PASS.

### Task 11: UI — `SeedDialog`, menu entry, Run again, Jobs label

**Files:**
- Create: `ui/src/transfer/seed/SeedDialog.tsx`, `ColumnsTable.tsx`, `GeneratorOptions.tsx`, `SeedReview.tsx`, `rerunSeed.ts`, `seed.css`, `SeedDialog.test.tsx`
- Modify: `ui/src/store/dialogs.ts` (`| { kind: 'seedData'; connectionId: string; table: ObjectRef; params?: SeedRequest; rerun?: number }`), `ui/src/dialogs/DialogHost.tsx` (render branch + `import '../transfer/seed/rerunSeed'`), `ui/src/components/TreeContextMenu.tsx` (`seedActionItems(node)` after `importActionItems` in the table menu), `ui/src/components/TreeContextMenu.test.tsx`, `ui/src/jobs/jobFormat.ts` (`seed: 'Generate data'`), `ui/src/transfer/import/steps/ReviewStep.tsx` (`confirmationDone(preview: Pick<DataImportPreview, 'pre_step' | 'confirm'>, c)`)

**Interfaces:**
- Consumes: Task 10; `api.seedTarget/seedPreview/seedPrepare/seedStart`; `Modal`, `EnvTag`, `showJobs`, `useToasts`, `Confirmation`/`confirmationDone`.
- Produces: `SeedDialog({ connectionId, table, params?, onClose })` with `data-testid="seed-dialog"`, preview grid `data-testid="seed-preview"`, column rows `data-testid="seed-col-<name>"`, review `data-testid="seed-review"`.

- [ ] **Step 1: Write failing tests** (`SeedDialog.test.tsx`, mocking `api` as `ImportWizard.test.tsx` does):
  - renders title `Generate data — shop.orders` and one row per column with the inferred generator label; locked column's select disabled with title `Generated column: the server computes it.`
  - changing `email` to `Pattern` shows a pattern input; typing calls `seedPreview` once after 300 ms (fake timers) with the new plan.
  - an issue from `seedPreview` (`{column:'total', message:'Values must be below 1000.'}`) shows under that row and disables `Review…`.
  - NULL % input only on nullable columns; Unique checkbox hidden for `Bool`.
  - 🎲 (button name `New seed`) changes the Seed input; `Refresh` calls `seedPreview` with the same seed.
  - Review: `confirm.kind === 'type_name'` → `Generate` disabled until `orders` is typed; `click` → checkbox `I understand`; irreversible pre-step → checkbox `I understand existing rows are deleted even if the job fails.`; `Generate` calls `seedStart(planId, { confirmed: true, name: 'orders', irreversible_ack: … })`, closes, toast `Generating data… — see Jobs`.
  - `plan_expired` from `seedStart` → back to Configure with `The review expired; review again.`
  - `TreeContextMenu.test.tsx`: table menu has `Generate data…`; disabled on a read-only connection with hint `Read-only connection`; click requests `{ kind: 'seedData', connectionId, table }`.
  - rerun: `rerun({ kind: 'seed', request })` requests `seedData` with `params: request`.
- [ ] **Step 2: Run** `pnpm --dir ui exec vitest run src/transfer/seed src/components/TreeContextMenu.test.tsx` — expect FAIL.
- [ ] **Step 3: Implement** per spec §6.2 layout (Configure: options bar, columns table, preview grid; Review: summary `Insert 1,000 rows into shop.orders in 1 transaction (1 batch).` — counts via `formatCount` / plural `batch(es)`, `without a transaction` instead of `in 1 transaction` when not transactional — pre-step block with `Cannot be undone` label, sample INSERT, warnings, confirmation). Styles in `seed.css` with `--dnt-*` tokens only.
- [ ] **Step 4: Run** `pnpm --dir ui test` and `pnpm --dir ui lint` — expect PASS.

### Task 12: Browser e2e

**Files:**
- Create: `e2e-web/src/seedFixtures.ts` (canned `ordersSeedTarget(env, server, readOnly)`, `seedSample(request)`, `seedPreview(request, planId, env)`)
- Modify: `e2e-web/src/backend.ts` (append handlers `seed_target`, `seed_preview`, `seed_prepare`, `seed_start` mirroring `import_*`: unknown plan → `plan_expired`; missing typed name / ack → `confirmation_required`; read-only → `blocked`)
- Create: `e2e-web/specs/seed.spec.ts`

- [ ] **Step 1: Write the spec:**
  - `Generate data… on orders: change a generator, preview updates, review, generate; the Jobs panel shows the job Done` — expand `shop` › `dnt_fixture` › `Tables`, right-click `orders`, menu `Generate data…`, `seed-dialog` visible, change `status` generator to `Constant` with value `new` → `seed-preview` shows `new` in every row, `Review…`, `Generate`, open Jobs, job `Generate data for orders` reaches `Done` via a `job://progress` event (as `import-wizard.spec.ts` does).
  - `Production: Generate stays disabled until the table name is typed` — on `shop-prod`, `Delete first` selected → typed-name input; wrong name keeps `Generate` disabled; `orders` enables it; `seed_start` called with `name: 'orders'`.
  - `Read-only: the menu entry is disabled`.
- [ ] **Step 2: Run** `$env:E2E_WEB_PORT='5176'; pnpm --dir e2e-web test specs/seed.spec.ts` — expect FAIL before handlers exist, then implement the handlers/fixtures.
- [ ] **Step 3: Run** the same — expect PASS; then `pnpm --dir e2e-web test specs/import-wizard.spec.ts specs/jobs.spec.ts specs/tree.spec.ts` — expect PASS (no regressions); `pnpm --dir e2e-web typecheck` — PASS.

### Final verification

- [ ] `cargo test --workspace` — PASS.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` — PASS (or no new warnings in touched files if the tree already has some).
- [ ] `pnpm --dir ui test`, `pnpm --dir ui lint`, `pnpm --dir ui exec tsc --noEmit` — PASS.
- [ ] e2e-web suites of Task 12 — PASS.
- [ ] `git status`: only the seeder's files plus appended lines in shared files changed; nothing committed.
