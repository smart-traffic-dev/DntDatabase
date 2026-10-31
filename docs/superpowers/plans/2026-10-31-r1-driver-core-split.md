# R1 — Driver core split Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the core ready for non-SQL engines without changing any MySQL/PostgreSQL behaviour. That means:
- split `Session` into a shared `Session` and an `SqlSession`;
- add `Capabilities`;
- add `DbKind::{Mongo, Redis}`, with no driver registered for them;
- remove every two-way `DbKind` branch;
- gate the SQL-only commands on the backend.

**Architecture:** SQL code stops taking `DbKind` and takes a new two-variant `SqlKind`. The conversion happens once, at the boundary (`DbKind::require_sql()?`), so the SQL builders never meet a Mongo/Redis kind. `SqlSession: Session` is a subtrait: SQL helpers take `&dyn SqlSession`, and callers get one from `session.sql()?`. A ratchet test (`kind_branches`) starts with an allowlist of today's offending files. Each conversion task shrinks the list, and the last task deletes it.

**Tech Stack:** Rust 1.99 stable (trait upcasting), async-trait, serde/specta/tauri-specta, React 19 + TypeScript (bindings only), vitest.

**Spec:** `docs/superpowers/specs/2026-10-31-non-sql-driver-core-design.md`. Phase R1 is §8, and it implements §3, §7.1 (Rust side) and §7.2 (backend side). § numbers below refer to the spec.

## Global Constraints

- **No behaviour change for MySQL/PostgreSQL.**
  - Every existing test passes. Tests may only change mechanically: `DbKind::X` becomes `SqlKind::X` where the callee now takes `SqlKind`; fakes split their `impl Session`; `ServerInfo` literals use the constructor.
  - Insta snapshots stay unchanged (no `.snap.new` files).
  - Error and message strings stay byte-identical.
- **Start from a clean tree.** The working tree currently holds uncommitted work from other sessions (data seeder, UI changes, `bindings.ts`, …). The user commits it first; this plan then runs in a fresh worktree off that commit. Never stage or revert files this plan does not touch.
- **Dependencies:** no new crate or npm package.
- **Language:** identifiers, error text and UI copy are in English.
- **Bindings:** after any change to an exported type, run `cargo run -p dntdatabase --example export_bindings`; `cargo test -p dntdatabase --test commands bindings_are_current` must then pass. `ui/src/bindings.ts` may only gain what each task names: `SqlKind`, `Capabilities` / `EngineFamily` / `NamespaceModel`, `ServerInfo.capabilities`, and the new `DbKind` / `ServerVariant` variants.
- **`SqlKind` vs `DbKind` in exported types:** a struct or enum that derives `specta::Type` and appears in `ui/src/bindings.ts` keeps any `DbKind` field it has; convert where the field is used. Function parameters, locals and non-exported fields in SQL code take `SqlKind`.
- **`match` arms:** no `_ =>` arm may merge kinds. Every `match` on `DbKind` / `SqlKind` lists each variant, or an explicit `A | B` group.
- **Verification per task:** `cargo check --workspace --all-targets`, plus the task's own tests, plus `cargo test -p dntdatabase --test kind_branches`. Run the full CI set only in Task 12.
- **Commit style:** `refactor(<area>): …` / `feat(core): …`, one commit per task.
- **Deviations from the spec's code sketch, decided here:**
  1. `SqlKind` is a new type.
  2. `SqlSession: Session` is a subtrait.
  3. In R1, `Engine` has only the `Sql` variant; R2 adds `Command`, `CommandSession`, `Session::keyspace()` and `dyn Session::command()`. The spec's `require_family(…, Sql)` is `require_sql`, which also returns the `SqlKind`.
  4. `ConnectionConfig::validate` rejects Mongo/Redis until R3.
  5. New `Cap` enum and `Capabilities::for_kind`.

## Review Focus

1. **Messages rebuilt from `DbKind::label()` must equal the old literals**, for example `MySQL has no schemas inside a database` and `a schema is required on PostgreSQL`. Pinned by Task 10 test `namespace_errors_keep_their_text`.
2. **`SqlKind` leaking into exported DTOs** would change TS types under the UI. Task 8 and Task 10 each end with a `git diff ui/src/bindings.ts` check against the allowed additions.
3. **An `_ =>` arm that routes Mongo/Redis into a SQL branch.** The lint cannot see these. Task 10 step 6 lists every remaining `match` on a kind with a wildcard arm and fixes it.
4. **Gating must read the live session's capabilities when connected**, and the per-kind defaults otherwise. Pinned by Task 11 test `gating_uses_live_capabilities`.
5. **A `.dntconn` holding a `mongo` connection** (possible now that `DbKind` deserialises it) must fail with a config error on `kind`, not an SQLite CHECK failure, and insert nothing. Pinned by Task 10 test `import_rejects_unsupported_kind`.

---

### Task 1: `SqlKind` type

**Files:**
- Modify: `crates/core/src/server.rs`

**Interfaces:**
- Produces:
  - `pub enum SqlKind { MySql, Postgres }`: derives `Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, specta::Type`, with `#[serde(rename_all = "lowercase")]`.
  - `impl From<SqlKind> for DbKind`.
  - Temporary `impl DbKind { pub fn as_sql(self) -> SqlKind }`: infallible while `DbKind` has two variants; Task 10 deletes it. Its doc comment says so.

- [ ] **Step 1: Write failing tests** in `server.rs` tests:

```rust
#[test]
fn sql_kind_serializes_like_db_kind() {
    assert_eq!(serde_json::to_value(SqlKind::MySql).unwrap(), "mysql");
    assert_eq!(serde_json::to_value(SqlKind::Postgres).unwrap(), "postgres");
    assert_eq!(DbKind::from(SqlKind::Postgres), DbKind::Postgres);
    assert_eq!(DbKind::MySql.as_sql(), SqlKind::MySql);
    assert_eq!(DbKind::Postgres.as_sql(), SqlKind::Postgres);
}
```

- [ ] **Step 2: Run** `cargo test -p dnt-core sql_kind_serializes_like_db_kind`. Expected: compile error, `SqlKind` not found.
- [ ] **Step 3: Implement** the interfaces above.
- [ ] **Step 4: Run** the test again. Expected: PASS.
- [ ] **Step 5: Commit** `feat(core): SqlKind for SQL-only code`.

### Task 2: `kind_branches` ratchet test

**Files:**
- Create: `src-tauri/tests/kind_branches.rs`

**Interfaces:**
- Produces:
  - `fn offends(line: &str) -> bool`, local to the test file.
  - `const ALLOWED: &[&str]`: workspace-relative paths with `/` separators.
  - Later tasks remove entries from `ALLOWED`.

Scan rules:
- Walk `crates/*/src/**/*.rs` and `src-tauri/src/**/*.rs`, rooted at `concat!(env!("CARGO_MANIFEST_DIR"), "/..")`.
- Skip a file when its name contains `tests` or it sits under a `tests` directory.
- Within a file, stop at the first line whose trimmed text is `#[cfg(test)]`.
- Skip lines whose trimmed text starts with `//`.
- A line offends when it contains any of these, with `K` standing for `DbKind` or `SqlKind`:
  - `== K::` or `!= K::` (whitespace allowed around the operator);
  - `K::<Variant>` followed by optional whitespace and then `==` or `!=`;
  - `if let K::`;
  - `matches!(` together with `K::` on the same line.

- [ ] **Step 1: Write the tests**

```rust
#[test]
fn offends_detects_two_way_branches() {
    for line in [
        "if kind == DbKind::MySql {",
        "    kind != SqlKind::Postgres",
        "DbKind::MySql == kind",
        "matches!(kind, DbKind::MySql)",
        "if let SqlKind::MySql = kind {",
    ] {
        assert!(offends(line), "{line}");
    }
    for line in [
        "match kind { DbKind::MySql => 1, DbKind::Postgres => 2 }",
        "let k = DbKind::MySql;",
        "// if kind == DbKind::MySql",
    ] {
        assert!(!offends(line), "{line}");
    }
}

#[test]
fn no_binary_kind_branches() {
    // offending: BTreeSet<String> of files with ≥ 1 offending line (scan rules above)
    // assert offending == ALLOWED (as a set). On failure, print "new offenders: …"
    // and "stale ALLOWED entries: …" separately.
}
```

- [ ] **Step 2: Run** `cargo test -p dntdatabase --test kind_branches` with `ALLOWED = &[]`. Expected: `offends_detects_two_way_branches` PASS; `no_binary_kind_branches` FAIL, listing the current offenders.
- [ ] **Step 3: Fill `ALLOWED`** with exactly the printed new offenders, sorted.
- [ ] **Step 4: Run** the same command. Expected: PASS.
- [ ] **Step 5: Commit** `test: ratchet against two-way DbKind branches`.

### Task 3: `dnt-core` SQL code takes `SqlKind`

**Files:**
- Modify: `crates/core/src/sql/{ident,ddl,filter,build,lexer}.rs`, `crates/core/src/sql/ddl_edit/{mod,build,header,template}.rs`, `crates/core/src/tx.rs`, and the tests in those files.
- Modify: callers in other crates. These only add `.as_sql()` or `.into()` at the call site; they get no other edits in this task.
- Modify: `src-tauri/tests/kind_branches.rs`. Remove every `crates/core/` entry from `ALLOWED`.

**Interfaces:**
- Consumes: `SqlKind`, `DbKind::as_sql`, `From<SqlKind> for DbKind` (Task 1).
- Produces:
  - `Dialect::for_kind(kind: SqlKind) -> Dialect`;
  - `quote_ident(kind: SqlKind, name: &str) -> String`;
  - `qualified_name(kind: SqlKind, database: &str, schema: Option<&str>, name: &str) -> String`;
  - `validate_identifier(kind: SqlKind, …)`, `action_sql(kind: SqlKind, …)`;
  - `build.rs` `kind_of(d: Dialect) -> SqlKind`;
  - in general, every `DbKind` parameter, local or non-exported field in these files becomes `SqlKind`, per the Global Constraints rule.

- [ ] **Step 1: Remove the `crates/core/` entries** from `ALLOWED` and run `cargo test -p dntdatabase --test kind_branches`. Expected: FAIL, new offenders in `crates/core/…`.
- [ ] **Step 2: Retype** the listed files from `DbKind` to `SqlKind`, keeping exported types as they are. Rewrite each two-way `if` / `matches!` on the kind as an exhaustive `match` on `SqlKind`.
- [ ] **Step 3: Fix call sites** across the workspace until `cargo check --workspace --all-targets` passes, using `kind.as_sql()` (DbKind → SqlKind) or `.into()` (SqlKind → DbKind).
- [ ] **Step 4: Run** `cargo test -p dnt-core` and `cargo test -p dntdatabase --test kind_branches`. Expected: PASS, and no new insta files (`git status --short | Select-String '\.snap\.new'` prints nothing).
- [ ] **Step 5: Commit** `refactor(core): SQL builders take SqlKind`.

### Task 4: `dnt-query` takes `SqlKind`

**Files:**
- Modify: `crates/query/src/{edit,plan,explain}.rs` and the other `crates/query/src` files the compiler names, plus their tests.
- Modify: callers (`.as_sql()` only).
- Modify: `kind_branches.rs` `ALLOWED`. Remove the `crates/query/` entries.

**Interfaces:**
- Consumes: Task 3 signatures.
- Produces: `atomicity(detail: &TableDetail, kind: SqlKind) -> Atomicity`, and every other `DbKind` parameter in `crates/query/src` becomes `SqlKind` under the same rule.

- [ ] **Step 1: Remove the `crates/query/` entries** from `ALLOWED`; run `kind_branches`. Expected: FAIL.
- [ ] **Step 2: Retype and rewrite** the branches, as in Task 3.
- [ ] **Step 3: Fix call sites** until `cargo check --workspace --all-targets` passes.
- [ ] **Step 4: Run** `cargo test -p dnt-query` and `kind_branches`. Expected: PASS, no `.snap.new`.
- [ ] **Step 5: Commit** `refactor(query): take SqlKind`.

### Task 5: `dnt-schema` takes `SqlKind`

**Files:**
- Modify: `crates/schema/src/**` (`render/{mod,mysql,pg}.rs`, `fidelity.rs`, `from_detail.rs`, `types.rs`, `validate.rs`, `lib.rs`) and the tests.
- Modify: callers (`.as_sql()` only).
- Modify: `ALLOWED`. Remove the `crates/schema/` entries.

**Interfaces:**
- Consumes: Task 3.
- Produces: same rule for every `DbKind` in `crates/schema/src`. Exported designer types keep `DbKind`.

- [ ] **Step 1: Remove the `crates/schema/` entries** from `ALLOWED`; run `kind_branches`. Expected: FAIL.
- [ ] **Step 2: Retype and rewrite** the branches.
- [ ] **Step 3: Run** `cargo check --workspace --all-targets`. Expected: PASS.
- [ ] **Step 4: Run** `cargo test -p dnt-schema` and `kind_branches`. Expected: PASS, no `.snap.new`.
- [ ] **Step 5: Commit** `refactor(schema): take SqlKind`.

### Task 6: `dnt-transfer` and `dnt-testkit` take `SqlKind`

**Files:**
- Modify: `crates/transfer/src/**` (`dump.rs`, `convert.rs`, `detect.rs`, `tools.rs`, `seed/{engine,generator,limits,infer}.rs`, …) and their tests (`detect_tests.rs`, `tools_tests.rs`, `crates/transfer/tests/*`).
- Modify: `crates/testkit/src/lib.rs`, `crates/testkit/src/bin/seed_dev.rs`.
- Modify: `ALLOWED`. Remove the `crates/transfer/` and `crates/testkit/` entries.

**Interfaces:** same rule. Exported import/dump DTOs keep `DbKind`.

- [ ] **Step 1: Remove the entries** from `ALLOWED`; run `kind_branches`. Expected: FAIL.
- [ ] **Step 2: Retype and rewrite** the branches.
- [ ] **Step 3: Run** `cargo check --workspace --all-targets`. Expected: PASS.
- [ ] **Step 4: Run** `cargo test -p dnt-transfer`, `cargo test -p dnt-testkit` and `kind_branches`. Expected: PASS.
- [ ] **Step 5: Commit** `refactor(transfer): take SqlKind`.

### Task 7: `src-tauri` SQL modules take `SqlKind`

**Files:**
- Modify (whole modules, tests included):
  - `src-tauri/src/backup/`, `import_data/`, `seed_data/`, `table_design/`, `ddl_edit/`;
  - `object_actions.rs`, `edit_plans.rs`, `ddl_exec.rs`, `export_format.rs`, `query_sessions.rs`, `data_sessions.rs`, `run_events.rs`;
  - `commands/{data,export,browse,query,objects,table_design,ddl_edit,import_data,backup,seed}.rs`.
- Not in this task: `session_manager.rs`, `state.rs`, `commands/connections.rs`, `commands/transfer.rs`. They stay on `DbKind` until Task 10.
- Modify: `ALLOWED`. It keeps only files outside this task's list.

**Interfaces:**
- Consumes: Tasks 3–6.
- Produces: every function in these modules takes `SqlKind`. Where a module reads `cfg.kind` / `info.kind`, it converts with `.as_sql()` at that line; Task 10 turns those into `require_sql()?`. Mark each such line with `// R1: boundary` so Task 10 can find them with grep.

- [ ] **Step 1: Remove** this task's files from `ALLOWED`; run `kind_branches`. Expected: FAIL.
- [ ] **Step 2: Retype and rewrite** the branches. Tag each `cfg.kind.as_sql()` / `info.kind.as_sql()` line with `// R1: boundary`.
- [ ] **Step 3: Run** `cargo check --workspace --all-targets`. Expected: PASS.
- [ ] **Step 4: Run** `cargo test -p dntdatabase` and `kind_branches`. Expected: PASS.
- [ ] **Step 5: Commit** `refactor(app): SQL modules take SqlKind`.

### Task 8: `Capabilities` on `ServerInfo`

**Files:**
- Modify: `crates/core/src/server.rs`, `crates/core/src/lib.rs` if a re-export is needed.
- Modify: every Rust `ServerInfo { … }` literal (core `driver.rs`, `sql/ddl_edit/mod.rs`, schema `lib.rs`, transfer `tools_tests.rs`, query tests, src-tauri tests and fakes). Each becomes `ServerInfo::new(…)`.
- Modify: `ui/src/bindings.ts` (regenerated) and every TS `ServerInfo` literal (`ui/src/test/fixtures/{data,seed}.ts`, the `*.test.ts(x)` files that `tsc` names). Create `ui/src/test/fixtures/caps.ts` exporting `sqlCaps(namespaces: NamespaceModel): Capabilities` for those literals.

**Interfaces:**
- Produces (in `server.rs`):

```rust
#[serde(rename_all = "snake_case")] pub enum EngineFamily { Sql, Command }
#[serde(rename_all = "snake_case")] pub enum NamespaceModel { DbSchema, Db, NumberedDb }
pub struct Capabilities { /* fields exactly as spec §3.1 */ }
pub enum Cap { DataEdit, Designer, DdlEdit, ObjectActions, Import, Dump, Seed, Explain }
impl Cap { pub fn label(self) -> &'static str }
impl Capabilities {
    pub fn sql_full(namespaces: NamespaceModel) -> Capabilities;
    pub fn for_kind(kind: DbKind) -> Capabilities;
    pub fn has(&self, cap: Cap) -> bool;
}
pub fn check_cap(caps: &Capabilities, cap: Cap) -> Result<(), DbError>;
impl ServerInfo {
    pub fn new(kind: DbKind, variant: ServerVariant, version: (u32, u32, u32),
               version_string: impl Into<String>) -> ServerInfo;
}
pub struct ServerInfo { /* existing fields */ pub capabilities: Capabilities }
```

- Details:
  - `Cap::label` returns `"data editing"`, `"table designer"`, `"DDL editing"`, `"object actions"`, `"import"`, `"dump and restore"`, `"data generation"`, `"explain"`.
  - `sql_full`: family `Sql`, every flag `true` except `paged_objects = false`.
  - `for_kind`: MySQL → `sql_full(Db)`, Postgres → `sql_full(DbSchema)`.
  - `check_cap` errors with `DbError::Unsupported { feature: cap.label().into(), version: String::new() }`.
  - `ServerInfo::new` sets `capabilities: Capabilities::for_kind(kind)`. `detect_mysql` and `detect_pg` build through it.
  - All four new types derive `Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type` (`Cap` needs no serde/specta: it is not exported).
  - `impl Cap { pub const ALL: [Cap; 8] }` lists every variant, in declaration order.

- [ ] **Step 1: Write failing tests** in `server.rs`:

```rust
#[test]
fn sql_capabilities() {
    let c = Capabilities::sql_full(NamespaceModel::Db);
    assert_eq!((c.family, c.namespaces, c.paged_objects), (EngineFamily::Sql, NamespaceModel::Db, false));
    assert!(c.transactions && c.data_edit && c.designer && c.ddl_edit && c.object_actions
        && c.import && c.dump && c.seed && c.explain);
    assert_eq!(Capabilities::for_kind(DbKind::Postgres), Capabilities::sql_full(NamespaceModel::DbSchema));
    assert_eq!(detect_mysql("8.0.36").unwrap().capabilities.namespaces, NamespaceModel::Db);
    assert_eq!(detect_pg(170002, "PostgreSQL 17.2").capabilities.namespaces, NamespaceModel::DbSchema);
    let j = serde_json::to_value(Capabilities::sql_full(NamespaceModel::DbSchema)).unwrap();
    assert_eq!((j["family"].as_str(), j["namespaces"].as_str()), (Some("sql"), Some("db_schema")));
}

#[test]
fn check_cap_names_the_feature() {
    let mut c = Capabilities::sql_full(NamespaceModel::Db);
    assert!(Cap::ALL.iter().all(|&cap| c.has(cap) && check_cap(&c, cap).is_ok()));
    c.designer = false;
    assert!(matches!(check_cap(&c, Cap::Designer),
        Err(DbError::Unsupported { feature, version }) if feature == "table designer" && version.is_empty()));
}
```

- [ ] **Step 2: Run** `cargo test -p dnt-core sql_capabilities check_cap_names_the_feature`. Expected: compile error.
- [ ] **Step 3: Implement** the interfaces. Replace every `ServerInfo { … }` literal with `ServerInfo::new(…)`. A test that needs custom capabilities sets the field after `new`.
- [ ] **Step 4: Run** `cargo test --workspace --no-run`, then `cargo test -p dnt-core`. Expected: PASS.
- [ ] **Step 5: Regenerate bindings and fix the TS literals.** Run `cargo run -p dntdatabase --example export_bindings`, add `capabilities: sqlCaps(…)` to each TS literal `tsc` reports, then run `pnpm --dir ui lint` and `pnpm --dir ui test`. Expected: PASS.
- [ ] **Step 6: Check the bindings diff.** Run `git diff ui/src/bindings.ts`. It may only add `EngineFamily`, `NamespaceModel`, `Capabilities` and `ServerInfo.capabilities`. Also run `cargo test -p dntdatabase --test commands bindings_are_current`. Expected: PASS.
- [ ] **Step 7: Commit** `feat(core): driver capabilities on ServerInfo`.

### Task 9: split `Session` into `Session` + `SqlSession`

**Files:**
- Modify: `crates/core/src/driver.rs` (traits, default `execute`, tests).
- Modify: `crates/driver-mysql/src/session.rs`, `crates/driver-pg/src/session.rs`.
- Modify: `crates/query/src/run.rs`; `run` and `run_with` take `session: &dyn SqlSession`.
- Modify: every `src-tauri/src` helper that calls a SQL method (the `&dyn Session` parameters listed by `rg "dyn Session" src-tauri/src`). These take `&dyn SqlSession`; their callers obtain one with `.sql()?`.
- Modify: the 16 test fakes: `crates/query/tests/run.rs`, `src-tauri/tests/{session_manager,query_sessions,jobs,data_sessions,data_edit}.rs`, `src-tauri/src/{table_design,import_data,ddl_edit,backup}/tests.rs`, `src-tauri/src/backup/restore_tests.rs`, and core `driver.rs` tests.

**Interfaces:**
- Consumes: `Capabilities` (Task 8).
- Produces (in `driver.rs`). Doc comments move with each method unchanged; `execute_one`'s doc gains nothing yet.

```rust
#[async_trait]
pub trait Session: Send + Sync {
    fn server_info(&self) -> &ServerInfo;
    fn capabilities(&self) -> &Capabilities { &self.server_info().capabilities }
    fn engine(&self) -> Engine<'_>;
    fn introspector(&self) -> &dyn Introspector;
    fn cancel_handle(&self) -> CancelHandle;
    fn reconnects(&self) -> u64 { 0 }
    async fn ping(&self) -> Result<(), DbError>;
    async fn close(self: Box<Self>);
}
pub enum Engine<'a> { Sql(&'a dyn SqlSession) }   // R2 adds Command
#[async_trait]
pub trait SqlSession: Session {
    fn dialect(&self) -> Dialect;
    fn tx_state(&self) -> TxState;
    fn in_transaction(&self) -> bool { self.tx_state() != TxState::Idle }
    async fn execute_one(&self, stmt: &str, kind: StatementKind, opts: &ExecOptions,
                         sink: &mut dyn RowSink) -> Result<StmtOutcome, DbError>;
    async fn execute(&self, sql: &str, opts: ExecOptions,
                     sink: &mut dyn RowSink) -> Result<ExecSummary, DbError>; // body: today's default
}
impl dyn Session + '_ {
    pub fn sql(&self) -> Result<&dyn SqlSession, DbError>;
}
```

  Each implementor writes `fn engine(&self) -> Engine<'_> { Engine::Sql(self) }`.

- [ ] **Step 1: Write failing tests** in `driver.rs` tests. Move the existing `FakeSession` methods into an `impl SqlSession`, then add:

```rust
#[test]
fn sql_returns_the_same_session() {
    let s: Box<dyn Session> = Box::new(fake());
    let sql = s.sql().unwrap();
    assert!(std::ptr::addr_eq(sql as *const dyn SqlSession, &*s as *const dyn Session));
    assert_eq!(sql.dialect(), Dialect::Postgres);
    assert!(!sql.in_transaction());
    assert_eq!(s.capabilities().family, EngineFamily::Sql);
}
```

  Keep `default_execute_runs_statements_until_the_first_error`, calling `execute` through `SqlSession`.

- [ ] **Step 2: Run** `cargo test -p dnt-core sql_returns_the_same_session`. Expected: compile error.
- [ ] **Step 3: Implement** the traits in core.
- [ ] **Step 4: Split the implementations.** In the MySQL and PG drivers, split each impl into `impl Session` + `impl SqlSession`; method bodies are unchanged. Do the same for the 16 fakes.
- [ ] **Step 5: Update callers.** Retype `query::run` and the `src-tauri` helpers to `&dyn SqlSession`; at each place that holds a `Box<dyn Session>` / `&dyn Session` and calls into SQL, add `let sql = session.sql()?;`. Repeat until `cargo check --workspace --all-targets` passes.
- [ ] **Step 6: Run** `cargo test --workspace`. Without Docker, the DB-backed integration tests skip as they do today. Expected: PASS, no `.snap.new`.
- [ ] **Step 7: Commit** `refactor(core): split Session into Session + SqlSession`.

### Task 10: `DbKind::{Mongo, Redis}`

**Files:**
- Modify: `crates/core/src/server.rs`, `crates/core/src/config.rs`.
- Modify: `crates/store/src/{repo,ncx}.rs`; tests in `crates/store/src/transfer.rs` or `crates/store/tests/`.
- Modify: `src-tauri/src/session_manager.rs`, plus every `// R1: boundary` line from Task 7.
- Modify: `ui/src/bindings.ts` (regenerated); `ui/src/dialogs/ConnectionDialog.tsx` (`ENGINE_NAME`) and any other TS map `tsc` names.
- Modify: `ALLOWED`. Fix and remove every remaining entry (`session_manager.rs`, `state.rs`, `commands/connections.rs`, …). `ALLOWED` is empty after this task.

**Interfaces:**
- Consumes: `SqlKind` (Task 1), `Capabilities` (Task 8).
- Produces:

```rust
pub enum DbKind { MySql, Postgres, Mongo, Redis }        // serde "mongo", "redis"
pub enum ServerVariant { MySql57, MySql8, MariaDb, Postgres, Mongo, Redis }
impl DbKind {
    pub fn family(self) -> EngineFamily;         // MySql | Postgres → Sql; Mongo | Redis → Command
    pub fn label(self) -> &'static str;          // "MySQL", "PostgreSQL", "MongoDB", "Redis"
    pub fn sql(self) -> Option<SqlKind>;
    pub fn require_sql(self) -> Result<SqlKind, DbError>;
    // Err(DbError::Unsupported { feature: format!("SQL on {}", self.label()), version: String::new() })
}
```

  `DbKind::as_sql` is deleted.

- `Capabilities::for_kind` gains:
  - `Mongo` → `{ family: Command, namespaces: Db, data_edit: true, every other flag false }`;
  - `Redis` → `{ family: Command, namespaces: NumberedDb, data_edit: true, paged_objects: true, every other flag false }`.
- `MySqlCapabilities::for_server`: `ServerVariant::Mongo | ServerVariant::Redis` → `Self::default()`.
- `ConnectionConfig::validate` becomes a `match self.kind`:
  - MySQL and PG keep today's checks.
  - `Mongo | Redis` → `DbError::Config { field: Some("kind"), message: format!("{} connections are not supported yet", self.kind.label()) }`. R3 replaces this.
- `repo.rs` maps `"mongo"` ↔ `Mongo` and `"redis"` ↔ `Redis`. The SQLite CHECK is unchanged in R1; validate stops these kinds first.
- `ncx.rs`: `default_port` gives Mongo 27017 and Redis 6379. `db_kind` is unchanged, so Navicat Mongo/Redis entries are still skipped as today.
- `session_manager.rs`. Exhaustive `match`es, with values:

| Function | MySql | Postgres | Mongo | Redis |
|---|---|---|---|---|
| `home` (constructor) | `None` | `default_db` or username (as today) | `None` | `None` |
| `metadata_key` | `None` | as today | `None` | `None` |
| `home_database` | `cfg.default_db` | `home` | `cfg.default_db` | `cfg.default_db` |
| object kinds in `completion_objects` | as today | as today | `&[]` | `&[]` |
| `load_schema_cache` | as today | as today | `Err(Unsupported { feature: "schema cache" })` | same |

  The schema rule at the top of `SessionManager::completion_objects` switches on `Capabilities::for_kind(kind).namespaces`:
  - `(Db | NumberedDb, Some(_))` → `config_error("schema", &format!("{} has no schemas inside a database", kind.label()))`;
  - `(DbSchema, None)` → `config_error("schema", &format!("a schema is required on {}", kind.label()))`.

- [ ] **Step 1: Write failing tests**

```rust
// server.rs
#[test]
fn new_kinds() {
    for (k, s) in [(DbKind::Mongo, "mongo"), (DbKind::Redis, "redis")] {
        assert_eq!(serde_json::to_value(k).unwrap(), s);
        assert_eq!(serde_json::from_value::<DbKind>(s.into()).unwrap(), k);
        assert_eq!((k.family(), k.sql()), (EngineFamily::Command, None));
    }
    assert_eq!(DbKind::Postgres.require_sql().unwrap(), SqlKind::Postgres);
    assert!(matches!(DbKind::Mongo.require_sql(),
        Err(DbError::Unsupported { feature, .. }) if feature == "SQL on MongoDB"));
    assert_eq!([DbKind::MySql, DbKind::Postgres, DbKind::Mongo, DbKind::Redis].map(DbKind::label),
        ["MySQL", "PostgreSQL", "MongoDB", "Redis"]);
    let r = Capabilities::for_kind(DbKind::Redis);
    assert!(r.paged_objects && r.data_edit && !r.designer && !r.transactions);
    assert_eq!(r.namespaces, NamespaceModel::NumberedDb);
}
// config.rs
#[test]
fn validate_rejects_unsupported_kinds() {
    let mut c = sample_config();
    c.kind = DbKind::Redis;
    assert!(matches!(c.validate(), Err(DbError::Config { field: Some(f), message })
        if f == "kind" && message == "Redis connections are not supported yet"));
}
// session_manager.rs tests (or src-tauri/tests/session_manager.rs)
#[tokio::test]
async fn namespace_errors_keep_their_text() {
    // completion_objects on a connected MySQL fake with schema Some("x")
    //   → Config { field "schema", message "MySQL has no schemas inside a database" }
    // completion_objects on a connected Postgres fake with schema None
    //   → Config { field "schema", message "a schema is required on PostgreSQL" }
}
// store
#[test]
fn import_rejects_unsupported_kind() {
    // bundle with one connection of kind Mongo → transfer::apply returns
    // Err(DbError::Config { field: Some("kind"), .. }); repo.list() is unchanged.
}
```

- [ ] **Step 2: Run** `cargo test -p dnt-core new_kinds validate_rejects_unsupported_kinds`. Expected: compile error.
- [ ] **Step 3: Implement** the variants and helpers. Delete `as_sql`; at every `// R1: boundary` site, use `require_sql()?` and drop the marker. A boundary inside a function that does not return `Result<_, DbError>` makes that function return `Result`, or makes it take `SqlKind` from its caller. Repeat until `cargo check --workspace --all-targets` passes.
- [ ] **Step 4: Implement** the `session_manager`, `config`, `repo` and `ncx` arms from the table.
- [ ] **Step 5: Run** `cargo test --workspace` and `kind_branches`. Expected: PASS.
- [ ] **Step 6: Wildcard sweep.** Run `rg -n "match .*kind" -A12 crates src-tauri/src --type rust`, then look at every `_ =>` arm inside a `match` on `DbKind` / `SqlKind` (tests included). Replace each with explicit variants, then re-run step 5.
- [ ] **Step 7: Bindings and UI.**
  - Regenerate the bindings and add `Mongo: 'MongoDB'`, `Redis: 'Redis'` to `ENGINE_NAME` and to any other `Record<ServerVariant | DbKind, …>` that `tsc` reports.
  - `ENGINES` in `connectionDefaults.ts` stays at two entries: the picker must not offer Mongo/Redis in R1. Add the vitest `engine picker offers only SQL engines` in `ConnectionDialog.test.tsx`, asserting that `conn-engine-mysql` and `conn-engine-postgres` exist, and that `conn-engine-mongo` and `conn-engine-redis` do not.
  - Run `pnpm --dir ui lint` and `pnpm --dir ui test`. Expected: PASS.
  - `git diff ui/src/bindings.ts` may only add the `DbKind` and `ServerVariant` variants (Review Focus 2).
- [ ] **Step 8: Commit** `feat(core): DbKind::Mongo and DbKind::Redis (no driver yet)`.

### Task 11: backend gating

**Files:**
- Modify: `src-tauri/src/session_manager.rs`: `capabilities(id)`.
- Modify: `src-tauri/src/commands/mod.rs`: `require_sql`, `require_cap`.
- Modify: the `_impl` functions of the commands below.
- Test: `src-tauri/src/table_design/tests.rs` (or the existing fake harness closest to each command).

**Interfaces:**
- Consumes: `Cap`, `check_cap`, `Capabilities::for_kind` (Task 8); `DbKind::require_sql` (Task 10).
- Produces:
  - `SessionManager::capabilities(&self, id: Uuid) -> Result<Capabilities, DbError>` (sync): `runtime_opt(id)` is `Some` → that runtime's `server_info().capabilities`; `None` → `Capabilities::for_kind(self.repo.get(id)?.kind)`.
  - `pub(crate) fn require_sql(state: &AppState, id: Uuid) -> Result<SqlKind, DbError>`: `state.repo.get(id)?.kind.require_sql()`.
  - `pub(crate) fn require_cap(state: &AppState, id: Uuid, cap: Cap) -> Result<SqlKind, DbError>`: `require_sql`, then `check_cap(&state.sessions.capabilities(id)?, cap)`.
  - Gating table. Each check is the first statement of the `_impl` that receives a connection `id`. `_impl`s that get only a plan, edit or job id are not gated, because their plan was made by a gated call.

| Check | Commands |
|---|---|
| `require_cap(Designer)` | `table_design_open`, `table_design_render`, `table_design_preview` |
| `require_cap(DdlEdit)` | `ddl_edit_open`, `ddl_edit_preview` |
| `require_cap(ObjectActions)` | `object_action_preview`, `object_action_render`, `namespace_options` |
| `require_cap(Import)` | `import_target`, `import_auto_map`, `import_check_mapping`, `import_dry_run`, `import_prepare` |
| `require_cap(Dump)` | `dump_objects`, `dump_start`, `tool_dump_start`, `restore_preview` |
| `require_cap(Seed)` | `seed_target`, `seed_preview`, `seed_prepare` |
| `require_cap(Explain)` | `query_explain` |
| `require_cap(DataEdit)` | `data_editable`, `data_preview`, `data_cell_fetch` |
| `require_sql` | `table_data`, `table_row_count`, `data_copy_sql`, `export_rows`, `export_table`, `export_query`, `query_plan`, `query_run`, `statement_ranges`, `query_tx`, `query_tx_mode`, `query_plan_statement`, `completion_scope`, `completion_objects` |

  Where a gated `_impl` already derives a `SqlKind` from `cfg.kind`, it uses the value `require_*` returns instead.

- [ ] **Step 1: Write failing tests**

```rust
#[tokio::test]
async fn gating_uses_live_capabilities() {
    // fake SQL session whose ServerInfo::new(..) has capabilities.designer = false;
    // connect it, then table_design_open_impl(state, id, …) →
    // Err(DbError::Unsupported { feature, .. }) with feature == "table designer".
    // Same harness, designer = true → the call gets past the gate (whatever it returned before R1).
}

#[tokio::test]
async fn gating_falls_back_to_kind_defaults_when_disconnected() {
    // MySQL connection never opened: state.sessions.capabilities(id)
    //   == Capabilities::sql_full(NamespaceModel::Db)
}
```

- [ ] **Step 2: Run** `cargo test -p dntdatabase gating_`. Expected: FAIL. Either `capabilities` is missing, or `table_design_open_impl` passes the designer-off session.
- [ ] **Step 3: Implement** `capabilities`, `require_sql` and `require_cap`, and insert the gates from the table.
- [ ] **Step 4: Run** `cargo test -p dntdatabase`. Expected: PASS. No existing test changes, because every SQL capability is `true`.
- [ ] **Step 5: Commit** `feat(app): gate SQL-only commands on capabilities`.

### Task 12: close the ratchet, then full verification

**Files:**
- Modify: `src-tauri/tests/kind_branches.rs`.

- [ ] **Step 1: Close the ratchet.** `ALLOWED` must already be empty. Delete the constant and make `no_binary_kind_branches` assert that `offending` is empty.
- [ ] **Step 2: Run the CI set.** Expected: every command passes.

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
pnpm --dir ui lint
pnpm --dir ui test
```

- [ ] **Step 3: Run the DB-backed suite when Docker is available.** Expected: PASS, no `.snap.new`. If Docker is not available, say so in the hand-off; do not skip silently.

```bash
docker compose -f testdata/docker-compose.test.yml up -d --wait
cargo test --workspace -- --test-threads=1
docker compose -f testdata/docker-compose.test.yml down -v
```

- [ ] **Step 4: Run e2e-web.** `$env:E2E_WEB_PORT='5176'; pnpm --dir e2e-web test`. Expected: PASS.
- [ ] **Step 5: Commit** `test: forbid two-way kind branches`.
