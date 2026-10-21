# P4b Table Designer Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Create new tables and edit existing ones (columns, PK, indexes, FKs, checks, options, comment) through a designer tab, with byte-exact SQL preview, backend guard, and per-element locking of anything the app cannot round-trip.

**Architecture:** Introspection gains the fields DDL needs; a new pure crate `dnt-schema` turns `TableDetail` into an editable `TableDef`, checks fidelity against server DDL, diffs two defs and renders engine-specific SQL. `src-tauri/src/table_design.rs` holds designs and single-use plans and executes on a dedicated DDL session using execution plumbing extracted from P4a. The UI adds a `design` tab kind with a draft store, sub-tab editors, a live SQL preview and a review dialog.

**Tech Stack:** Rust (tokio, serde, specta, insta), Tauri 2 commands + tauri-specta bindings, React 18 + TypeScript + Zustand, Vitest, Playwright (e2e-web).

**Spec:** `docs/superpowers/specs/2026-10-21-p4b-table-designer-design.md`

**Model per task (for the SDD controller):** each task heading carries `[sonnet]` (mechanical, well-specified) or `[opus]` (design judgement, engine semantics, concurrency). Reviewers use the same model as the implementer.

## Global Constraints

- All UI copy is English. Chat/docs may be Vietnamese.
- Servers: MySQL 5.7 / 8.0 / 8.4, MariaDB 10.6 / 11.4, PostgreSQL 12 / 17 (`dnt_testkit::Target::all()` = 6, plus `MySql84` where a test covers it).
- Preview SQL is byte-identical to the SQL sent; apply sends the strings stored in the plan, never re-renders.
- Backend never accepts SQL from the UI, only `TableDef`.
- MySQL/MariaDB: all changes in ONE `ALTER TABLE` statement; new table = one `CREATE TABLE`.
- PostgreSQL: statements in one `BEGIN … COMMIT`, order: drop FK/check/index/PK → rename table → rename columns → alter columns → add columns → add PK/unique/index/check/FK → `COMMENT ON`.
- Confirmation: Production = type the exact table name (as returned by backend, no trim) for every change; Staging/Local/untagged = click Apply. Read-only connection blocks at backend.
- Lock wait 5 s per apply (PG `SET lock_timeout = '5s'`, MySQL `SET SESSION lock_wait_timeout = 5`); impact count queries: read-only, 2 s timeout each, failure = "not counted".
- No auto-retry of writes; `outcome_unknown` preserved to UI and History; History origin `table_design`.
- Locked elements never appear in a diff; designer has no drop-table; no online DDL options; PG column reorder hidden.
- No new runtime dependencies (insta only as dev-dependency, as in the drivers).
- Every new `u64` crossing IPC carries `#[specta(type = f64)]` / `Option<f64>`.

## Review Focus

1. A MySQL column with an attribute the app does not model (e.g. `SRID`, `COLUMN_FORMAT`, `STORAGE`, a charset differing from the table) must come out locked, never silently re-rendered without it → Task 9 test `mysql_unknown_column_attribute_locks_column`.
2. Renaming a column that a locked expression index or check references must be refused, not rendered → Task 6 test `rename_column_used_by_locked_index_is_refused`.
3. Identifiers with quotes, backticks, spaces, Unicode (`đơn_hàng`, ``we`ird``) must render correctly quoted on both engines → Task 7/8 tests `quotes_hostile_identifiers`.
4. Draft opened, table altered from another session, then Apply → `PlanStale`, zero statements sent → Task 11 test `concurrent_alter_makes_plan_stale`.
5. Closing the designer tab or the app with unsaved changes must ask, like staged data edits → Task 13 test `closing_dirty_design_tab_asks`.

---

### Task 1: Extend introspection types [sonnet]

**Files:**
- Modify: `crates/core/src/meta.rs`
- Modify (constructors only, fill new fields with defaults): `crates/driver-mysql/src/introspect.rs`, `crates/driver-pg/src/introspect.rs`, `crates/driver-pg/src/ddl.rs` (tests), `crates/core/src/sql/filter.rs`, `crates/query/src/edit.rs`, `src-tauri/src/commands/data.rs`, `src-tauri/tests/data_edit.rs`, `e2e-web/src/backend.ts` (`column()`, `index()`, `detailOf()`), `ui/src/bindings.ts` (regenerated)

**Interfaces:**
- Produces (all new fields `#[serde(default)]`, structs derive `Default` where missing):
  - `ColumnDetail`: `charset: Option<String>`, `on_update: Option<String>`, `identity_kind: Option<IdentityKind>`, `unsigned: bool`, `enum_values: Vec<String>`
  - `pub enum IdentityKind { Always, ByDefault }` (serde snake_case)
  - `IndexDetail`: `parts: Vec<IndexPart>`, `visible: bool` (default `true`), `include: Vec<String>`, `nulls_not_distinct: bool`, `comment: Option<String>`
  - `pub struct IndexPart { column: Option<String>, expression: Option<String>, prefix: Option<u32>, descending: bool }`
  - `ForeignKeyDetail`: `match_type: Option<String>`, `deferrable: bool`, `initially_deferred: bool`
  - `TableOptions`: `charset: Option<String>`, `row_format: Option<String>`, `auto_increment: Option<u64>` (`#[specta(type = Option<f64>)]`), `extra: Vec<(String, String)>`, `is_partition: bool`, `inherits: Vec<String>`

- [ ] **Step 1:** Add a unit test in `meta.rs` `index_detail_defaults_visible`: deserialising `{"name":"i","unique":false,"primary":false,"partial":false,"columns":["a"],"expression":null,"method":null,"definition":null}` yields `visible == true`, `parts.is_empty()`.
- [ ] **Step 2:** Run `cargo test -p dnt-core meta::` → FAIL (field missing).
- [ ] **Step 3:** Add the fields/types above; `visible` uses `#[serde(default = "yes")]`. Fill every literal constructor listed in Files with defaults (`..Default::default()` where the struct derives Default; explicit where not).
- [ ] **Step 4:** `cargo test --workspace` (without DB) → PASS; regenerate bindings: `DNT_UPDATE_FIXTURES=1 cargo test -p dntdatabase --test commands bindings_are_current`; `pnpm --dir ui lint` → clean; `pnpm --dir e2e-web typecheck` → clean.
- [ ] **Step 5:** Commit `feat(core): introspection fields for table DDL fidelity`.

### Task 2: MySQL/MariaDB fill the new fields [sonnet]

**Files:**
- Modify: `crates/driver-mysql/src/queries.rs` (`TABLE`, `COLUMNS`, `INDEXES`, `INDEXES_WITH_EXPRESSION`), `crates/driver-mysql/src/introspect.rs` (`column_detail`, `group_indexes`, `table_detail`)
- Test: `crates/driver-mysql/tests/introspect.rs`, snapshots `crates/driver-mysql/tests/snapshots/introspect__*`

**Interfaces:**
- Consumes: Task 1 fields.
- Produces: MySQL `TableDetail` with `charset` (`CHARACTER_SET_NAME`), `on_update` (from `EXTRA` `on update …`, text after `on update `), `unsigned` (`COLUMN_TYPE` contains ` unsigned`), `enum_values` (parsed from `enum(…)`/`set(…)`, `''` unescaped), index `parts` (`SUB_PART` → prefix, `COLLATION='D'` → descending, expression parts on 8.0+), `visible` (`IS_VISIBLE='YES'`, 8.0+ only; MariaDB `IGNORED='NO'` on 10.6+), `comment` (`INDEX_COMMENT`, empty → None), table `charset` (from collation via `information_schema.COLLATIONS`), `row_format` (`ROW_FORMAT`), `auto_increment` (`AUTO_INCREMENT`), `extra` (`CREATE_OPTIONS` split on spaces into `key=value`, excluding `partitioned`).

- [ ] **Step 1:** Add DB test `mysql_detail_has_ddl_fields` (all 4 mysql-family targets, `require_db!`): in a scratch table `CREATE TABLE t (id int unsigned NOT NULL AUTO_INCREMENT, ts timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP, s enum('a','b''c') NOT NULL, name varchar(100) CHARACTER SET latin1, PRIMARY KEY (id), KEY k_name (name(10)) COMMENT 'pfx') ROW_FORMAT=DYNAMIC`; assert `id.unsigned`, `ts.on_update == Some("CURRENT_TIMESTAMP")` (case-insensitive compare), `s.enum_values == ["a","b'c"]`, `name.charset == Some("latin1")`, index `k_name.parts[0].prefix == Some(10)`, `comment == Some("pfx")`, `options.row_format == Some("Dynamic")` (case-insensitive), `options.auto_increment.is_some()`. On MySql8/84 also: `KEY k_desc (name DESC)` → `descending`, `KEY k_inv (s) INVISIBLE` → `!visible`.
- [ ] **Step 2:** Run with `DNT_TEST_DB=1 cargo test -p dnt-driver-mysql --test introspect mysql_detail_has_ddl_fields` → FAIL.
- [ ] **Step 3:** Extend queries and mapping as in Produces; version branches via `self.capabilities()` / `server_info().variant`.
- [ ] **Step 4:** Test PASS; accept snapshot changes with `DNT_UPDATE_FIXTURES=1` / `cargo insta review` and check the diff only adds the new fields.
- [ ] **Step 5:** Commit `feat(driver-mysql): read charset, on update, index parts and table options`.

### Task 3: PostgreSQL fill the new fields [sonnet]

**Files:**
- Modify: `crates/driver-pg/src/queries.rs` (`relation()`, `COLUMNS`, `INDEXES`, `FOREIGN_KEYS`), `crates/driver-pg/src/introspect.rs` (`column_detail`, `index_details`, `foreign_keys`, `table_detail`)
- Test: `crates/driver-pg/tests/introspect.rs`, snapshots `crates/driver-pg/tests/snapshots/introspect__*`

**Interfaces:**
- Produces: `identity_kind` (attidentity `a`→Always, `d`→ByDefault; keep existing `identity` string), index `parts` (per key column from `indkey` with `indoption & 1` → descending; expression parts from `pg_get_indexdef(indexrelid, k, true)` when attnum = 0), `include` (columns with ord > `indnkeyatts`), `nulls_not_distinct` (`indnullsnotdistinct`, only when `server_info().version >= (15,0,0)`), `comment` (`obj_description(indexrelid,'pg_class')`), FK `match_type` (`confmatchtype` f/p/s → FULL/PARTIAL/SIMPLE), `deferrable`, `initially_deferred`, table `is_partition` (`relispartition`), `inherits` (`pg_inherits` parents, qualified `schema.name` only when schema ≠ table schema).

- [ ] **Step 1:** DB test `pg_detail_has_ddl_fields` (Pg12, Pg17): scratch schema with `CREATE TABLE p (id int GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, a text, b int); CREATE INDEX p_ab ON p (a DESC, b) INCLUDE (id); COMMENT ON INDEX p_ab IS 'x'; CREATE TABLE c (pid int REFERENCES p(id) MATCH FULL DEFERRABLE INITIALLY DEFERRED); CREATE TABLE child () INHERITS (p);` Assert `id.identity_kind == Some(ByDefault)`, `p_ab.parts[0].descending && !parts[1].descending`, `p_ab.include == ["id"]`, `p_ab.comment == Some("x")`, FK `match_type == Some("FULL")`, `deferrable && initially_deferred`, `child.options.inherits == ["p"]`; `sales.measurements` partitions report `is_partition`.
- [ ] **Step 2:** Run `DNT_TEST_DB=1 cargo test -p dnt-driver-pg --test introspect pg_detail_has_ddl_fields` → FAIL.
- [ ] **Step 3:** Extend queries/mapping.
- [ ] **Step 4:** PASS; update snapshots (only additive diffs); `ddl_roundtrip` still PASS.
- [ ] **Step 5:** Commit `feat(driver-pg): read identity kind, index parts, FK deferrability and inheritance`.

### Task 4: History origin `table_design` [sonnet]

**Files:**
- Modify: `crates/store/src/history.rs` (`HistoryOrigin`, `as_str`, `parse`), `crates/store/src/db.rs` (`MIGRATION_6`, `migrations()`), `ui/src/bindings.ts` (regenerated), any UI switch over origin (`ui/src/components/HistoryPanel.tsx` label "Table design")

**Interfaces:**
- Produces: `HistoryOrigin::TableDesign` ↔ `"table_design"`.

- [ ] **Step 1:** Tests in `db.rs` copying the migration-5 pattern: `migration_6_accepts_table_design_origin` (insert row with origin `table_design` succeeds; existing rows survive), and in `history.rs` `origin_round_trips_table_design`.
- [ ] **Step 2:** `cargo test -p dnt-store` → FAIL.
- [ ] **Step 3:** Add variant, MIGRATION_6 rebuilding `query_history` with `CHECK (origin IN ('query','data_edit','object_action','table_design'))`; History panel label.
- [ ] **Step 4:** `cargo test -p dnt-store` PASS; regenerate bindings; `pnpm --dir ui test HistoryPanel` PASS.
- [ ] **Step 5:** Commit `feat(store): table_design history origin`.

### Task 5: `dnt-schema` model, type catalog, from_detail, validate [opus]

**Files:**
- Create: `crates/schema/Cargo.toml` (copy `crates/query/Cargo.toml` shape; deps `dnt-core`, `serde`, `serde_json`, `specta`), `crates/schema/src/lib.rs`, `crates/schema/src/model.rs`, `crates/schema/src/types.rs`, `crates/schema/src/from_detail.rs`, `crates/schema/src/validate.rs`
- Modify: root `Cargo.toml` (`dnt-schema = { path = "crates/schema" }`), `crates/core/src/sql/build.rs` (add `check_expression`)

**Interfaces:**
- Produces (all `#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]`):
  - `TableDef { original_name: Option<String>, name: String, columns: Vec<ColumnDef>, primary_key: Option<KeyDef>, indexes: Vec<IndexDef>, foreign_keys: Vec<FkDef>, checks: Vec<CheckDef>, options: TableOptionsDef, comment: Option<String>, read_only_reason: Option<String> }`
  - `ColumnDef { original: Option<String>, name, data_type: TypeSpec, nullable: bool, default: Option<DefaultSpec>, auto_increment: bool, identity: Option<IdentityKind>, on_update: Option<String>, charset: Option<String>, collation: Option<String>, comment: Option<String>, using: Option<String>, generated: Option<String>, locked: bool }`
  - `#[serde(tag = "kind", rename_all = "snake_case")] enum TypeSpec { Known { base: String, length: Option<u32>, precision: Option<u32>, scale: Option<u32>, unsigned: bool, values: Vec<String> }, Custom { text: String } }`
  - `#[serde(tag = "kind", rename_all = "snake_case")] enum DefaultSpec { Value { text: String }, Expression { text: String } }`
  - `KeyDef { original: Option<String>, name: Option<String>, columns: Vec<String>, locked: bool }`
  - `IndexDef { original: Option<String>, name: String, unique: bool, parts: Vec<IndexPartDef>, method: Option<String>, include: Vec<String>, visible: bool, comment: Option<String>, locked: bool }`, `IndexPartDef { column: Option<String>, expression: Option<String>, prefix: Option<u32>, descending: bool }`
  - `FkDef { original: Option<String>, name: String, columns: Vec<String>, ref_schema: Option<String>, ref_table: String, ref_columns: Vec<String>, on_update: String, on_delete: String, deferrable: bool, initially_deferred: bool, locked: bool }`
  - `CheckDef { original: Option<String>, name: String, expression: String, locked: bool }`
  - `TableOptionsDef { engine: Option<String>, charset: Option<String>, collation: Option<String>, row_format: Option<String>, locked: bool }`
  - `TypeInfo { base: String, length: ParamUse, precision: ParamUse, scale: ParamUse, unsigned: bool, values: bool, collation: bool }`, `enum ParamUse { None, Optional, Required }`
  - `DraftError { element: ElementRef, field: Option<String>, message: String }`; `#[serde(tag="kind", content="name", rename_all="snake_case")] enum ElementRef { Table, Column(String), PrimaryKey, Index(String), ForeignKey(String), Check(String), Options }`
  - `pub fn type_catalog(server: &ServerInfo) -> Vec<TypeInfo>`
  - `pub fn parse_type(db_type: &str, server: &ServerInfo) -> TypeSpec` (unknown/array/domain → `Custom`)
  - `pub fn type_text(t: &TypeSpec, server: &ServerInfo) -> String` (canonical spelling, e.g. `decimal(14,2)`, `int unsigned`, `enum('a','b''c')`, PG `numeric(14,2)`, `character varying(20)` → `varchar(20)`)
  - `pub fn from_detail(detail: &TableDetail, server: &ServerInfo) -> TableDef` (every `original` = current name; `read_only_reason` set for partitioned, system-versioned, `is_partition`, non-empty `inherits`; generated columns and expression-index parts set `locked`)
  - `pub fn new_table(name: &str, server: &ServerInfo) -> TableDef` (one column `id`: MySQL `bigint unsigned` auto_increment, PG `bigint` identity ByDefault; PK on it)
  - `pub fn validate(def: &TableDef, server: &ServerInfo) -> Vec<DraftError>`
  - In core: `pub fn check_expression(text: &str, d: Dialect) -> Result<(), BuildError>` — generalises `check_raw_where` (same rules, forbidden list + `SELECT`), and `check_raw_where` calls it.

- [ ] **Step 1:** Tests (`crates/schema/src/*` `#[cfg(test)]`):
  - `parse_type_known`: MySQL `decimal(12,2)`→Known{base:"decimal",precision:12,scale:2}; `int unsigned`→unsigned; `enum('a','b''c')`→values `["a","b'c"]`; `varchar(16)`→length 16; PG `numeric(12,2)`, `character varying(20)`→base `varchar` length 20; `integer[]`→Custom; `order_status`→Custom.
  - `type_text_round_trips` for each of the above (`type_text(parse_type(x)) == canonical`).
  - `from_detail_marks_originals_and_locks`: generated column locked; partitioned table → `read_only_reason.is_some()`.
  - `validate_reports`: duplicate column name → `DraftError{element: Column("a"), field: Some("name")}`; empty name; table with zero columns → `Table`; `decimal` with scale > precision; FK column count ≠ ref count; CHECK on MySql57 → error "CHECK constraints need MySQL 8.0.16"; name failing `ddl::validate_identifier` → error; `DefaultSpec::Expression` failing `check_expression` → error; `Custom` type text containing `;` → error.
  - In core: `check_expression_rejects_statements` (`1; DROP TABLE t`, `(select 1)`, `now()` ok, `a > 0` ok).
- [ ] **Step 2:** `cargo test -p dnt-schema -p dnt-core` → FAIL.
- [ ] **Step 3:** Implement. Catalog lists (MySQL family): tinyint smallint mediumint int bigint decimal float double bit bool char varchar binary varbinary tinytext text mediumtext longtext tinyblob blob mediumblob longblob enum set date time datetime timestamp year json (+ MariaDB uuid on 10.7+ not in matrix, skip); PG: smallint integer bigint numeric real "double precision" boolean char varchar text bytea date time timetz timestamp timestamptz interval uuid json jsonb inet cidr macaddr. Identifier rules reuse `dnt_core::sql::ddl::validate_identifier`.
- [ ] **Step 4:** PASS; `cargo clippy -p dnt-schema --all-targets -- -D warnings` clean.
- [ ] **Step 5:** Commit `feat(schema): TableDef model, type catalog, from_detail and validation`.

### Task 6: `diff` [opus]

**Files:**
- Create: `crates/schema/src/diff.rs`

**Interfaces:**
- Consumes: Task 5 model.
- Produces:
  - `#[serde(tag="kind", rename_all="snake_case")] enum Change { CreateTable, RenameTable { from: String, to: String }, SetOptions { old: TableOptionsDef, new: TableOptionsDef }, SetComment { comment: Option<String> }, DropForeignKey { name: String }, DropCheck { name: String }, DropIndex { name: String }, DropPrimaryKey { name: Option<String> }, RenameColumn { from: String, to: String }, AlterColumn { name: String, old: ColumnDef, new: ColumnDef }, MoveColumn { name: String, position: Position }, DropColumn { name: String }, AddColumn { column: ColumnDef, position: Position }, AddPrimaryKey { key: KeyDef }, AddIndex { index: IndexDef }, RenameIndex { from: String, to: String }, AddCheck { check: CheckDef }, AddForeignKey { fk: FkDef } }`
  - `#[serde(tag="kind", content="column", rename_all="snake_case")] enum Position { First, After(String), Last }`
  - `enum DiffError { Locked { element: ElementRef, reason: String }, Invalid(Vec<DraftError>) }` (Serialize/Type)
  - `pub fn diff(old: &TableDef, new: &TableDef) -> Result<Vec<Change>, DiffError>`

Rules: match by `original` (None = new); old element whose `original` no longer appears in new = drop. Column rename + type change = `RenameColumn` then `AlterColumn` (name = new name). Index/FK/check whose content changed = drop + add (index name-only change = `RenameIndex`). Output order is exactly the enum order above (drops first, adds last), stable within a kind by old/new ordinal. `MoveColumn` only when relative order of kept columns changed (MySQL only; Task 8 PG render rejects it). Any change touching a `locked` element, or renaming/dropping a column referenced by a locked index part expression, locked check expression or generated column (word-boundary match of the column name in the expression text, case-insensitive) → `DiffError::Locked`.

- [ ] **Step 1:** Tests: `no_change_is_empty`; `rename_detected_via_original` (no drop/add); `rename_and_retype_column`; `dropped_and_added_column_order`; `index_content_change_is_drop_add`; `index_rename_only`; `fk_change_is_drop_add`; `move_column_after`; `locked_column_change_refused`; `rename_column_used_by_locked_index_is_refused` (Review Focus 2); `new_table_yields_create_table_only` (old `original_name == None`).
- [ ] **Step 2:** `cargo test -p dnt-schema diff` → FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** PASS.
- [ ] **Step 5:** Commit `feat(schema): diff two table definitions`.

### Task 7: MySQL/MariaDB render [opus]

**Files:**
- Create: `crates/schema/src/render/mod.rs`, `crates/schema/src/render/mysql.rs`

**Interfaces:**
- Consumes: Task 6 `Change`, Task 5 `type_text`, core `ident::quote_ident`, `build::literal`, `build::TableRef`.
- Produces:
  - `Rendered { statements: Vec<String>, impacts: Vec<Impact> }`, `Impact { kind: ImpactKind, element: ElementRef, message: String, count_sql: Option<String> }`, `enum ImpactKind { DataLoss, Rebuild, NotTransactional }` (serde snake_case)
  - `enum RenderError { Unsupported { feature: String, min_version: String }, Invalid { message: String } }`
  - `pub fn render(table: &TableRef, old: &TableDef, changes: &[Change], server: &ServerInfo) -> Result<Rendered, RenderError>` (dispatch by `server.kind`)
  - `pub fn render_create(table: &TableRef, def: &TableDef, server: &ServerInfo) -> Result<Rendered, RenderError>`
  - `pub fn column_sql(c: &ColumnDef, server: &ServerInfo) -> Result<String, RenderError>` and `pub fn index_sql`, `fk_sql`, `check_sql` (element fragments; Task 9 reuses them)

Rules: one `ALTER TABLE <qualified> ` + comma-joined clauses in `Change` order; `RenameTable` → `RENAME TO`; rename+alter column → `CHANGE COLUMN old new <full def>`; alter → `MODIFY COLUMN <full def>`; move → `MODIFY COLUMN … FIRST|AFTER x`; add → `ADD COLUMN … [FIRST|AFTER x]`; drop PK → `DROP PRIMARY KEY`; index rename → `RENAME INDEX a TO b`; options → `ENGINE=`, `DEFAULT CHARSET=`, `COLLATE=`, `ROW_FORMAT=`; comment → `COMMENT=`. Column def order: type, `CHARACTER SET`, `COLLATE`, `NOT NULL|NULL`, `DEFAULT`, `ON UPDATE`, `AUTO_INCREMENT`, `COMMENT`. Default Value → `literal` on a text value typed by the column (numbers unquoted after numeric check); Expression → verbatim in parentheses on MySQL 8.0.13+ unless it is `CURRENT_TIMESTAMP[(n)]`/`NULL`/a literal, else `Unsupported`. CHECK on MySql57 → `Unsupported{feature:"CHECK constraint", min_version:"MySQL 8.0.16"}`; DESC index part on MySql57 → `Unsupported{feature:"descending index", min_version:"MySQL 8.0"}`; INVISIBLE index on MySql57/MariaDB → `Unsupported`. Impacts: `DropColumn` → DataLoss with `count_sql` = `count_where_sql(relation, "<col> IS NOT NULL")`; `AlterColumn` nullable→not null → DataLoss count `<col> IS NULL`; length/precision shrink or type family change → DataLoss count `CHAR_LENGTH(<col>) > n` for strings, else count_sql None with message "may truncate values"; any type change or `MoveColumn` → Rebuild; engine not InnoDB → NotTransactional. `render_create`: one `CREATE TABLE` with columns, `PRIMARY KEY`, `KEY`/`UNIQUE KEY`, `CONSTRAINT … FOREIGN KEY`, `CONSTRAINT … CHECK`, table options.

- [ ] **Step 1:** Tests (exact strings): `alter_combines_into_one_statement` — board-10 case on MySql8: modify `total` to `decimal(14,2) NOT NULL DEFAULT '0.00'`, set comment on `shipped_at`, add `channel enum('web','app','pos') NOT NULL DEFAULT 'web' AFTER shipped_at`, drop `note` → exactly
  ``ALTER TABLE `dnt_fixture`.`orders` MODIFY COLUMN `total` decimal(14,2) NOT NULL DEFAULT '0.00', MODIFY COLUMN `shipped_at` datetime NULL COMMENT 'Ngày giao cho đơn vị vận chuyển', DROP COLUMN `note`, ADD COLUMN `channel` enum('web','app','pos') NOT NULL DEFAULT 'web' AFTER `shipped_at` `` (clauses follow the `Change` enum order: AlterColumn, DropColumn, AddColumn), plus impacts `[DataLoss(Column("note")) with count_sql, Rebuild(Column("total"))]`.
  - `rename_column_uses_change`; `rename_table_inside_alter`; `check_unsupported_on_57`; `quotes_hostile_identifiers` (``we`ird`` → ``we``ird``, `đơn_hàng`); `no_backslash_escapes_default_literal` (default `a\b` with `Dialect::MySql{no_backslash_escapes:true}` vs false); `create_table_full` (columns + PK + index + FK + check, one statement).
- [ ] **Step 2:** `cargo test -p dnt-schema render::mysql` → FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** PASS.
- [ ] **Step 5:** Commit `feat(schema): render MySQL/MariaDB ALTER and CREATE TABLE`.

### Task 8: PostgreSQL render [opus]

**Files:**
- Create: `crates/schema/src/render/pg.rs`

**Interfaces:**
- Consumes/produces: same functions as Task 7, PG branch.

Rules: one statement per clause in Global Constraints order; `ALTER TABLE <q> RENAME TO`, `RENAME COLUMN`, `ALTER COLUMN x TYPE <t> [USING …]`, `SET/DROP NOT NULL`, `SET/DROP DEFAULT`, `ADD/DROP IDENTITY`/`SET GENERATED`, `ADD COLUMN`, `DROP COLUMN`, `ADD CONSTRAINT … PRIMARY KEY|UNIQUE|FOREIGN KEY … [DEFERRABLE [INITIALLY DEFERRED]]|CHECK`, `DROP CONSTRAINT`, non-constraint index via `CREATE [UNIQUE] INDEX name ON <q> USING method (…) [INCLUDE (…)]` / `DROP INDEX <schema>.name`, `ALTER INDEX … RENAME TO`, `COMMENT ON TABLE|COLUMN … IS …|NULL`. Unique index vs unique constraint: keep the form the original used (`IndexDef` from a constraint has `definition` starting `UNIQUE (`/`PRIMARY KEY (` — carry a `constraint: bool` on `IndexDef`, add it in this task to Task 5's struct with default false and set it in `from_detail`). `MoveColumn` → `RenderError::Unsupported{feature:"column order", min_version:"not supported by PostgreSQL"}`. Auto `USING col::<type>` only for pairs in `SAFE_CASTS` (integer family widening/narrowing, numeric↔integer, any → text/varchar, varchar↔text, timestamp↔timestamptz, json→jsonb, text→json/jsonb/uuid/date/timestamp/numeric/integer); outside the list, require `ColumnDef.using`, else `RenderError::Invalid{message:"Changing <a> to <b> needs a USING expression"}`. Same impacts as Task 7 (no Rebuild for add nullable column without default; Rebuild for type change).

- [ ] **Step 1:** Tests: `statement_order_is_fixed` (drop fk, rename col, alter type, add col, add index, comment → exact vector of 6 strings); `type_change_auto_using` (`integer`→`bigint` → `… TYPE bigint USING "qty"::bigint`); `type_change_needs_using` (`text`→`integer[]` without using → Invalid); `move_column_unsupported`; `unique_constraint_kept_as_constraint`; `quotes_hostile_identifiers`; `create_table_full_pg` (CREATE TABLE + COMMENT ON + CREATE INDEX as separate statements).
- [ ] **Step 2:** `cargo test -p dnt-schema render::pg` → FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** PASS.
- [ ] **Step 5:** Commit `feat(schema): render PostgreSQL ALTER and CREATE TABLE`.

### Task 9: Fidelity check [opus]

**Files:**
- Create: `crates/schema/src/fidelity.rs`, `crates/schema/tests/fixtures/` (real `SHOW CREATE TABLE` outputs + matching `TableDetail` JSON for `orders`, `all_types`, `weird_names` from MySql57, MySql8, MariaDb106, MariaDb114; PG `orders`, `invoices`, `measurements` details from Pg12/Pg17 — capture with a `#[ignore]` helper test `capture_fixtures` that writes them when `DNT_UPDATE_FIXTURES=1`)

**Interfaces:**
- Consumes: Task 7/8 element fragment functions, Task 5 `from_detail`.
- Produces:
  - `Locked { element: ElementRef, reason: String, server_text: String }`
  - `pub fn fidelity(def: &TableDef, detail: &TableDetail, mysql_create: Option<&str>, server: &ServerInfo) -> Vec<Locked>`
  - `pub fn apply_locks(def: &mut TableDef, locks: &[Locked])` (sets `locked`; `ElementRef::Table` sets `read_only_reason`)

Rules: MySQL — split `SHOW CREATE TABLE` body into element lines (column lines start with a backquoted name; `PRIMARY KEY`, `UNIQUE KEY`, `KEY`, `FULLTEXT`/`SPATIAL KEY` (always lock), `CONSTRAINT … FOREIGN KEY`, `CONSTRAINT … CHECK`), table options from the trailer. Normalise both sides: collapse whitespace, lowercase keywords outside quotes, drop `DEFAULT NULL` on nullable columns, drop `CHARACTER SET`/`COLLATE` equal to the table's, drop integer display width (`int(11)`→`int`, 5.7), drop `USING BTREE`, MariaDB `current_timestamp()`→`CURRENT_TIMESTAMP`, strip trailing `,`. Compare the rendered fragment of each element with its server line; mismatch → `Locked{reason:"DntDatabase cannot reproduce this definition exactly"}`; a server line with no model element (e.g. FULLTEXT/SPATIAL index) is added to the def as a locked `IndexDef`, so it is shown and never touched. Unparseable trailer options → lock `Options`. PG — compare `index_sql`/`check_sql`/`fk_sql` against `IndexDetail.definition`/`CheckDetail.definition`/`ForeignKeyDetail.definition` after the same normalisation (`pg_get_*def` spelling: `USING btree`, casts like `'x'::text`); column compare `type_text` vs `db_type` and default vs `ColumnDetail.default`.

- [ ] **Step 1:** Tests: `fixtures_have_no_locks_for_plain_tables` (each `orders`/`weird_names` fixture → empty); `all_types_locks_only_expected` (assert the exact locked element list per server, written after capturing fixtures); `mysql_unknown_column_attribute_locks_column` (Review Focus 1: hand-edited fixture line with `SRID 4326` / `COLUMN_FORMAT DYNAMIC` → that column locked, others not); `fulltext_index_shown_locked`; `pg_partitioned_measurements_is_read_only`; `normalise_mariadb_current_timestamp`.
- [ ] **Step 2:** Capture fixtures (`DNT_TEST_DB=1 DNT_UPDATE_FIXTURES=1 cargo test -p dnt-schema --test fidelity capture_fixtures -- --ignored`), then `cargo test -p dnt-schema fidelity` → FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** PASS.
- [ ] **Step 5:** Commit `feat(schema): fidelity check against server DDL with per-element locks`.

### Task 10: Backend commands and shared DDL execution [opus]

**Files:**
- Create: `src-tauri/src/ddl_exec.rs` (extracted from `object_actions.rs`: operation registry `DdlOperations` with `register`/`OperationGuard`/precancel, `execute`, outcome mapping, history write, post-success cache refresh), `src-tauri/src/table_design.rs`, `src-tauri/src/commands/table_design.rs`
- Modify: `src-tauri/src/object_actions.rs` (use `ddl_exec`), `src-tauri/src/session_manager.rs` (field `table_designs: crate::table_design::DesignStore`; close drops designs and cancels operations), `src-tauri/src/lib.rs`, `src-tauri/src/commands/mod.rs` (register commands), `src-tauri/Cargo.toml` (`dnt-schema.workspace = true`), `ui/src/bindings.ts`

**Interfaces:**
- Consumes: Tasks 4–9.
- Produces (commands; `#[tauri::command] #[specta::specta]`, bodies `guard(state.sessions.x(..)).await`):
  - `table_design_open(connection_id: Uuid, scope: Scope, table: Option<String>) -> DesignOpen` where `DesignOpen { design_id: Uuid, def: TableDef, locked: Vec<Locked>, catalog: Vec<TypeInfo>, server: ServerInfo, env: Option<EnvTag>, read_only: bool }`
  - `table_design_render(design_id: Uuid, draft: TableDef) -> DesignRender` where `DesignRender { statements: Vec<String>, impacts: Vec<Impact>, errors: Vec<DraftError>, error: Option<String> }` (Unsupported/Invalid/Locked mapped to `error` text, never an `AppError`)
  - `table_design_preview(design_id: Uuid, draft: TableDef) -> DesignPreview` where `DesignPreview { plan_id: Uuid, statements: Vec<String>, impacts: Vec<CountedImpact>, confirm_name: Option<String>, expires_in_seconds: u32 }`, `CountedImpact { impact: Impact, rows: Option<f64>, counted: bool }`
  - `table_design_apply(plan_id: Uuid, confirmed: bool, name: Option<String>, operation_id: Uuid, channel: Channel<String>) -> DesignReport` where `DesignReport { outcome: ActionOutcome, error: Option<AppError>, warnings: Vec<String>, statements_sent: u32 }`
  - `table_design_cancel(operation_id: Uuid) -> ()`
  - `table_design_close(design_id: Uuid) -> ()`
  - `DesignStore` (MAX 64 designs; each holds connection id, `ObjectRef`/scope, base `TableDef`, `ServerInfo`, fingerprint, runtime generation)

Behaviour: open — read-only connection still opens (designer shows view-only); reads `table_detail_fresh` + `object_ddl` (MySQL `SHOW CREATE TABLE` text) on a query session under `METADATA_CALL_TIMEOUT`; fingerprint = hex of `std::hash::DefaultHasher` over `serde_json` of detail (row_estimate/size_bytes/auto_increment cleared) + ddl text. render — pure, from stored base. preview — `Blocked` when read-only; re-reads detail+ddl, fingerprint mismatch → `PlanStale{reason:"Table changed"}`; validate/diff/render; runs each `count_sql` read-only with 2 s timeout (`tokio::time::timeout`), failure → `counted:false`; `confirm_name = (env == Production).then(table name)`; judges every statement with `dnt_query::plan` + `guard_with` (any Block → `Blocked`); stores plan (TTL `plan_ttl`, MAX 128). apply — as `object_action_apply` but: re-check fingerprint on the DDL session; MySQL sends the single statement without BEGIN; PG `BEGIN`, statements in order with `progress("executing n/m")`, `COMMIT`; outcome mapping from `ddl_exec`; one History entry per apply with all statements joined by `;\n` and origin `TableDesign`; on any `sent`, refresh tree + schema cache and drop edit plans/loaded pages for the connection, and replace the design's base with a fresh open (so the UI can reload).

- [ ] **Step 1:** Unit tests with core `FakeSession` (in `table_design.rs` `#[cfg(test)]`, following the `object_actions.rs` tests): `preview_blocks_read_only`; `preview_detects_changed_table` (fake detail differs → PlanStale, no statements); `plan_is_single_use`; `production_requires_exact_name` (`"Orders"` vs `"orders"` → ConfirmationRequired); `pg_failure_mid_way_rolls_back` (fake fails 2nd statement → `RolledBack`, `statements_sent == 2`); `commit_lost_is_unknown`; `count_timeout_is_not_counted`. Existing `object_actions` tests still PASS after extraction.
- [ ] **Step 2:** `cargo test -p dntdatabase table_design object_actions` → FAIL.
- [ ] **Step 3:** Extract `ddl_exec.rs`, implement store/commands, register, regenerate bindings.
- [ ] **Step 4:** `cargo test -p dntdatabase` PASS; `bindings_are_current` PASS; clippy clean.
- [ ] **Step 5:** Commit `feat(app): table design commands on shared DDL execution`.

### Task 11: DB integration tests on 6 servers [opus]

**Files:**
- Create: `src-tauri/tests/table_design_db.rs` (helpers copied from `object_actions_db.rs`: `app_state`, `open`, `sql`; unique scratch namespace `p4b_{uuid}` with `catch_unwind` cleanup)

**Interfaces:**
- Consumes: Task 10 commands via `state.sessions.table_design_*`.

- [ ] **Step 1:** Tests (`require_db!`, loop `Target::all()` plus `MySql84`):
  - `round_trip_fixture_tables`: for `orders`, `customers`, `weird_names`, `all_types` (MySQL family) and `public.orders`, `public.customers`, `sales.invoices` (PG): open → no lock except the expected list from Task 9 → `render_create` into scratch namespace under a new name → open the copy → `TableDef` equal to the original (ignoring names/originals) and no locks.
  - `apply_each_change_kind`: in scratch table with rows, one apply per change kind supported on that server (add/drop/rename/alter column, move (MySQL), PK change, add/drop/rename index, add/drop FK, add/drop check where supported, options, comment); after each, re-open and assert the def matches the draft; rename column keeps row values.
  - `locked_element_untouched`: scratch table with an expression index (MySql8/PG) or FULLTEXT (MySQL family); alter another column; `SHOW CREATE TABLE`/`pg_get_indexdef` text of the locked element byte-identical before/after.
  - `concurrent_alter_makes_plan_stale` (Review Focus 4): preview, then `ALTER TABLE … ADD COLUMN zz int` from a direct driver session, then apply → `PlanStale`, column added by the draft absent.
  - `pg_error_rolls_back_all`: draft adds a column and sets NOT NULL on a column holding NULL → apply error, outcome `RolledBack`, added column absent.
  - `lock_wait_times_out`: hold `LOCK TABLES … WRITE` / `LOCK TABLE … IN ACCESS EXCLUSIVE MODE` in another session → apply fails within ~5 s, outcome `NotApplied`/`RolledBack`.
  - `impact_counts`: drop a column with 3 non-null values → `rows == Some(3.0)`.
- [ ] **Step 2:** `docker compose -f testdata/docker-compose.test.yml up -d --wait`; `DNT_TEST_DB=1 cargo test -p dntdatabase --test table_design_db` → FAIL until fixes land (fix bugs in Tasks 5–10 code here, with a unit test for each).
- [ ] **Step 3:** Fix until green.
- [ ] **Step 4:** PASS on all targets; record the output in the commit body.
- [ ] **Step 5:** Commit `test(app): table designer on all servers`.

### Task 12: UI design tab, draft store, tree entry points, close guard [sonnet]

**Files:**
- Create: `ui/src/store/tableDesigns.ts`, `ui/src/store/tableDesigns.test.ts`
- Modify: `ui/src/store/tabs.ts` (`DesignTabRef { id; kind: 'design'; connectionId; scope: Scope; table: string | null; title; pinned?; preview? }`, `Tab` union, `designTabId(connectionId, scope, table)`), `ui/src/components/EditorArea.tsx` (render `<DesignerTab>` placeholder), every `tab.kind` switch listed by the survey (`TabStripActions.tsx`, `TabBar.tsx` dirty dot + label "Design", `tabActions.ts` close + `unsaved`, `StatusBar.tsx`, `HistoryPanel.tsx`, `focusGroup.ts`, `resultExport.ts`, `store/queryTabs.ts` `closeTab` and `installCloseGuard`), `ui/src/components/TreeContextMenu.tsx` ("Design table" on tables, "New table…" on database/schema; disabled when connection closed), `ui/src/test/backend.ts` (`resetStores` includes the new store)

**Interfaces:**
- Consumes: bindings `TableDef`, `DesignOpen`, `DesignRender`, `api.tableDesignOpen/Render/Close`.
- Produces: `useTableDesigns` (Zustand), per tab id: `{ open: DesignOpen | null; draft: TableDef | null; past: TableDef[]; future: TableDef[]; selection: ElementRef | null; render: DesignRender | null; loading: boolean; error: string | null }`; actions `load(tabId, connectionId, scope, table)`, `edit(tabId, recipe: (d: TableDef) => TableDef)` (pushes undo, clears redo, schedules render with 300 ms debounce), `undo(tabId)`, `redo(tabId)`, `select(tabId, el)`, `discard(tabId)`, `reload(tabId)` (re-open after apply/stale), `dispose(tabId)` (calls `tableDesignClose`), selectors `isDirty(tabId)` (draft deep-unequal to `open.def`), `changeCount(tabId)` (= `render.statements` change count from a diff of top-level elements: added + removed + modified elements).

- [ ] **Step 1:** Tests: `edit_pushes_undo_and_renders_debounced` (fake timers; one `table_design_render` call after 300 ms for three quick edits); `undo_redo`; `dirty_and_change_count`; `discard_restores_open_def`; tree menu test `design_table_item_opens_design_tab` and `new_table_item_on_schema`; `closing_dirty_design_tab_asks` (Review Focus 5: `closeTab` returns false when the user cancels the `discardEdits` question; app close guard asks too).
- [ ] **Step 2:** `pnpm --dir ui test tableDesigns TreeContextMenu tabActions` → FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** PASS; `pnpm --dir ui lint` clean.
- [ ] **Step 5:** Commit `feat(ui): design tab kind, draft store and entry points`.

### Task 13: Designer tab — header, Columns grid, inspector, SQL preview [opus]

**Files:**
- Create: `ui/src/tabs/designer/DesignerTab.tsx`, `DesignerHeader.tsx`, `ColumnsGrid.tsx`, `ColumnInspector.tsx`, `SqlPreview.tsx`, `designer.css`, `DesignerTab.test.tsx`
- Modify: `ui/src/components/EditorArea.tsx` (replace placeholder)

**Interfaces:**
- Consumes: Task 12 store; bindings `TypeInfo`, `ElementRef`, `Impact`.
- Produces: `<DesignerTab tab={DesignTabRef} />`; sub-tab switch state `subTab: 'columns' | 'indexes' | 'foreign_keys' | 'checks' | 'options' | 'comment'` kept in the store per tab; `<SqlPreview>` exported for Task 15's dialog.

Board 10 layout (tokens `--dnt-*` only; focus 1 px; flat): header (name, `connection › database · engine`, segmented sub-tabs with counts, chip "{n} unsaved changes", Discard, primary "Review & apply…" disabled when `errors.length || render.error || !dirty || read_only || read_only_reason`); Columns grid rows: drag handle (hidden on Postgres), PK key icon, Name, Type, Length, Null checkbox, Default, Comment; row states modified (accent-subtle bg), new (success bg, "+"), to-drop (struck through, "Undo" link), locked (lock icon, tooltip `reason` + `server_text`, inputs disabled); "Add column" (`Ctrl+Enter`); keyboard `Ctrl+↑/↓` move (MySQL), `Delete` marks drop, `Ctrl+Z`/`Ctrl+Shift+Z`. Inspector shows fields per `TypeInfo` (`length`/`precision`/`scale` inputs only when not `None`, Unsigned only when `unsigned`, Collation only when `collation`), "Before: {old type}", Default mode toggle Value / Expression, USING input (Postgres, shown when type changed). SqlPreview: monospace numbered lines, data-loss statement lines tinted danger, Copy, "Open in query" (opens a new query tab with the SQL), impacts list beside; `render.error` shown as an inline alert. Read-only view: banner "This table is shown read-only: {reason}".

- [ ] **Step 1:** Tests (mock backend): `renders_columns_and_preview`; `type_change_shows_precision_fields`; `locked_column_is_disabled_with_reason`; `drag_handle_hidden_on_postgres`; `review_disabled_while_errors`; `draft_error_shown_on_cell`; `undo_shortcut`.
- [ ] **Step 2:** `pnpm --dir ui test DesignerTab` → FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** PASS; lint clean.
- [ ] **Step 5:** Commit `feat(ui): designer columns grid, inspector and live SQL preview`.

### Task 14: Indexes, Foreign keys, Checks, Options, Comment editors [sonnet]

**Files:**
- Create: `ui/src/tabs/designer/IndexesGrid.tsx`, `ForeignKeysGrid.tsx`, `ChecksGrid.tsx`, `OptionsPanel.tsx`, `CommentPanel.tsx`, `ElementsGrid.test.tsx`
- Modify: `ui/src/tabs/designer/DesignerTab.tsx` (wire sub-tabs)

**Interfaces:**
- Consumes: Task 12 store `edit`, Task 13 row-state styling classes.

Each grid: same row states and locked behaviour as Columns. Indexes: name, unique, parts (column picker + prefix (MySQL) + DESC), method (Postgres select), include (Postgres), visible (MySQL 8 only), comment. FKs: name, columns, referenced schema/table (text input with schema-cache completion), referenced columns, ON UPDATE/ON DELETE selects (`NO ACTION, RESTRICT, CASCADE, SET NULL, SET DEFAULT`), deferrable + initially deferred (Postgres). Checks: name, expression (monospace); tab disabled with tooltip "CHECK constraints need MySQL 8.0.16" on MySQL 5.7. Options: engine (MySQL; list from server `SHOW ENGINES` is out of scope — fixed list InnoDB, MyISAM, MEMORY, ARCHIVE, Aria (MariaDB)), charset, collation, row format; Postgres shows only table name. Comment: textarea. Table rename lives in Options as "Table name".

- [ ] **Step 1:** Tests: `add_index_with_desc_part`; `fk_actions_select`; `checks_tab_disabled_on_57`; `options_engine_change_marks_dirty`; `rename_table_in_options`.
- [ ] **Step 2:** `pnpm --dir ui test ElementsGrid` → FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** PASS; lint clean.
- [ ] **Step 5:** Commit `feat(ui): designer index, foreign key, check, option and comment editors`.

### Task 15: Review & apply dialog [sonnet]

**Files:**
- Create: `ui/src/dialogs/TableDesignDialog.tsx`, `ui/src/dialogs/TableDesignDialog.test.tsx`
- Modify: `ui/src/store/dialogs.ts` (`UiRequest` gains `{ kind: 'tableDesign'; tabId: string }`, state `tableDesignRunning: boolean`), `ui/src/dialogs/DialogHost.tsx`, `ui/src/store/queryTabs.ts` (`installCloseGuard` also checks `tableDesignRunning`)

**Interfaces:**
- Consumes: `api.tableDesignPreview/Apply/Cancel`, Task 12 `reload`, Task 13 `SqlPreview`, P4a `refreshAfterAction` pattern (tree refresh, `invalidateObjectData`, `invalidateTableData`, `invalidateCompletion`, `replaceObjectTabs` on table rename).

Flow: on open call preview (sequence counter like ObjectActionDialog); show statements (read-only `SqlPreview`), counted impacts ("412 rows have a value" / "Not counted"), rebuild/not-transactional notes; if `confirm_name` → input "Type {name} to apply" (PRODUCTION badge in header), Apply enabled only on exact match; else Apply enabled. Apply: `crypto.randomUUID()`, `tableDesignRunning = true`, `Channel` stage text, Stop → `tableDesignCancel`. Outcomes: Applied "Changes applied." → close, `reload`; NotApplied/RolledBack → show server error, keep draft; Unknown "The result is unknown. The table structure was reloaded; check it before continuing." → `reload`; `plan_stale` error → message "The table changed since you opened it." with button "Reopen" (keeps the old draft in the store under `staleDraft` for comparison; add `staleDraft: TableDef | null` to Task 12's per-tab state in this task).

- [ ] **Step 1:** Tests: `production_requires_exact_name`; `local_applies_on_click`; `rolled_back_keeps_draft`; `unknown_reloads`; `stale_offers_reopen`; `stop_calls_cancel`.
- [ ] **Step 2:** `pnpm --dir ui test TableDesignDialog` → FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** PASS; `pnpm --dir ui test` all PASS; lint clean.
- [ ] **Step 5:** Commit `feat(ui): review and apply dialog for the table designer`.

### Task 16: e2e-web coverage [sonnet]

**Files:**
- Modify: `e2e-web/src/backend.ts` (handlers `table_design_open/render/preview/apply/cancel/close` with a canned MySQL 8 `orders` design; apply returns Applied and bumps a column), Create: `e2e-web/specs/designer.spec.ts`

- [ ] **Step 1:** Spec: open tree → context menu "Design table" on `orders` → designer tab shows 8 columns → change `total` precision → SQL preview contains `MODIFY COLUMN \`total\` decimal(14,2)` → Review & apply → Applied → tab not dirty. Second test: PROD connection requires typing `orders`.
- [ ] **Step 2:** `pnpm e2e:web -- designer` → FAIL.
- [ ] **Step 3:** Implement handlers.
- [ ] **Step 4:** `pnpm e2e:web` all PASS.
- [ ] **Step 5:** Commit `test(e2e-web): table designer flows`.

### Task 17: Verification and final review notes [controller]

- [ ] `cargo fmt --all --check`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo test --workspace`.
- [ ] `DNT_TEST_DB=1 cargo test --workspace` with the docker matrix up (sequential); no skipped DB test counted as verified.
- [ ] `pnpm --dir ui lint`; `pnpm --dir ui test`; `pnpm e2e:web`.
- [ ] Desktop smoke (MySQL 8, PostgreSQL 17): New table, edit column/index/FK, apply; light + dark screenshots compared with board 10.
- [ ] Write `docs/superpowers/plans/2026-10-21-p4b-final-review.md` (environment, commands, results, known gaps); commit.
