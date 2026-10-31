# R2 — Command family core Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the core everything a non-SQL ("Command") driver plugs into, proven end-to-end by an in-memory fake driver:
- the command-language contract;
- a shared guard policy;
- a plan/guard/run pipeline for command scripts;
- a keyspace API and a data-access API;
- the R1 carry-overs that must land before any Command session exists.

No app/UI wiring yet; that is R4.

**Architecture:** `Engine` gains `Command(&dyn CommandSession)`, and the driver supplies the language through `CommandLanguage`.
- **Guard:** `dnt_query::policy` is the single source of truth for verdicts. A matrix test proves it matches the existing SQL guard.
- **Pipeline:** `dnt_query::command` mirrors `plan` / `guard` / `run` for commands.
- **Testing:** `dnt_testkit::fake_command` is a Redis-like in-memory driver. Every new path is tested through it, including app-side paths. The fake reports `DbKind::Postgres`, so the store accepts its connection, but it returns a Command session.

**Tech Stack:** Rust 1.99 stable, async-trait, serde/specta/tauri-specta, tokio. UI only for regenerated bindings.

**Spec:** `docs/superpowers/specs/2026-10-31-non-sql-driver-core-design.md`. R2 is §8. It implements §4, the types of §5.1–5.3, §7.4 and the §9 tests. Also read `docs/superpowers/plans/2026-10-31-r1-followups.md` ("Do early in R2").

## Global Constraints

- **MySQL/PostgreSQL behaviour is unchanged**, with one exception: Task 1's empty-version message fix. Every other existing test passes unchanged, except mechanical import-path updates, and insta snapshots do not change.
- **No new crate or npm package.**
- **Identifiers, error text and UI copy are in English.**
- **Bindings:** after any change to an exported type, run `cargo run -p dntdatabase --example export_bindings`; `cargo test -p dntdatabase --test commands bindings_are_current` must pass. In R2, `ui/src/bindings.ts` may only gain `ObjectKind` `"Collection"` and `"Key"` and `StmtKindLabel` `"admin"`. Moving types between crates must not change it in any other way.
- **`src-tauri/tests/kind_branches.rs` stays green:** no `== DbKind::` / `if let` / `matches!` kind branches.
- **Safety promises (spec §1) hold for the Command family:**
  - a read-only connection never receives a write;
  - Production confirms;
  - there are no automatic retries;
  - fail-closed: a parse error becomes Block, and an unknown command is Block on read-only and needs the typed name on Production.
- **Verification per task:** `cargo check --workspace --all-targets`, the task's own tests, `kind_branches`, `cargo fmt --all`, and `cargo clippy --workspace --all-targets -- -D warnings`. Task 10 runs the full set.
- **Commit style:** `feat(core|query|testkit|app): …` or `refactor(…)`, one commit per task.
- **Decisions this plan makes that the spec leaves open:**
  1. `CellInput`, `RowRef`, `RowEdit` and `EditSet` move to `dnt_core::edit`; `dnt_query::edit` re-exports them. This is needed because core's `DataAccess` uses them.
  2. `DataAccess` returns `DbError`, not `EditError`, which is SQL-specific.
  3. The fields of `KeyEdit` (the spec names the type only).
  4. The fake driver's toy language.
  5. dnt-schema's fallible public functions stay as they are (the R1 follow-up item is dropped under YAGNI).
  6. The two-layer gate placement and the extra repo reads stay as they are.

## Review Focus

1. **An unknown or unparseable command inside a multi-command script.** Nothing in the script may run when any command is Block. Pinned by Task 6 test `parse_error_blocks_the_whole_script` and Task 7 test `blocked_plan_sends_nothing`.
2. **`SELECT 2` partway through a script.** Later commands must run against db 2, and the run must report the change. Pinned by Task 8 test `select_changes_db_for_the_rest`.
3. **Stop raised before a command starts.** The command is never sent, and the error is `Cancelled { outcome_unknown: false }`. Pinned by Task 8 test `stop_before_start_sends_nothing`.
4. **`MULTI` with no `EXEC` at the end of a run.** The session must report `TxState::Active`, and nothing is committed implicitly. Pinned by Task 8 test `open_multi_reports_active`.
5. **Non-ASCII text (`SET k "héllo"`, emoji keys).** The plan's UTF-16 ranges must match the editor. Pinned by Task 6 test `ranges_are_utf16`.

---

### Task 1: R1 carry-overs in the SQL path

**Files:**
- Modify: `crates/core/src/error.rs`
- Modify: `crates/core/src/driver.rs`
- Modify: `crates/schema/src/render/mod.rs`
- Modify: `src-tauri/src/session_manager.rs`
- Modify: the `.sql()?` call sites listed in r1-followups ("~21 `.sql()?` early returns")
- Modify: the `require_sql()?` re-derivation sites in `src-tauri/src/{ddl_edit/catalog.rs, ddl_edit/mod.rs, object_actions.rs}` and similar
- Modify: any test asserting the old empty-version text

**Interfaces:**
- Produces:
  - `DbError::Unsupported { feature, version }` with an empty `version` displays as `"<feature> is not supported"`; its detail is `"<feature>"`. A non-empty version keeps today's text.
  - `SqlSession::sql_kind(&self) -> SqlKind`: a default method returning `crate::sql::build::kind_of(self.dialect())`, or wherever `kind_of` now lives.
  - `pub(crate) async fn sql_or_close(session: Box<dyn Session>) -> Result<Box<dyn Session>, DbError>` in `session_manager.rs`. When `session.sql()` fails, it calls `close_quietly(session)` and returns that error; otherwise it returns the session unchanged.
  - `render::sql_kind` returns the same text as `DbKind::require_sql`'s error (`"SQL on <label>"`) inside its `RenderError::Invalid`.

- [ ] **Step 1: Write failing tests**
  - In `error.rs` tests:

    ```rust
    #[test]
    fn unsupported_without_version_omits_the_clause() {
        let e = DbError::Unsupported { feature: "table designer".into(), version: String::new() };
        assert_eq!(e.to_string(), "table designer is not supported");
        let v = DbError::Unsupported { feature: "server version".into(), version: "5.6".into() };
        assert_eq!(v.to_string(), NON_EMPTY_TEXT);
    }
    ```

    Before writing the test, record `NON_EMPTY_TEXT` by printing today's `Display` for the non-empty case, so the test pins that it does not change. Do the same for the `AppError` detail.
  - In `driver.rs` tests: `sql_kind_follows_dialect`. The fake PG session gives `SqlKind::Postgres`.
- [ ] **Step 2: Run** `cargo test -p dnt-core unsupported_without_version sql_kind_follows_dialect`. Expected: FAIL.
- [ ] **Step 3: Implement** the interfaces. Apply them at the call sites:
  - Every `open_query_session(..)` followed by `.sql()?` before `close_quietly` becomes `let session = sql_or_close(open_query_session(..).await?).await?;`.
  - Code that holds a `&dyn SqlSession` uses `.sql_kind()` instead of `server_info().kind.require_sql()?`.
  - Update every test that pinned an old empty-version text, and list each one in the report.
- [ ] **Step 4: Run** `cargo test --workspace` and `kind_branches`. Expected: PASS.
- [ ] **Step 5: Commit** `refactor(core): R1 carry-overs (unsupported text, sql_kind, sql_or_close)`.

### Task 2: Command engine, command contract, `Redacted` in core

**Files:**
- Create: `crates/core/src/command.rs`, `crates/core/src/redact.rs`
- Modify: `crates/core/src/lib.rs`, `crates/core/src/driver.rs`
- Modify: `crates/query/src/redact.rs` (re-export)
- Modify: `src-tauri/src/query_sessions.rs` and `src-tauri/src/data_sessions.rs` (the "no SqlSession ⇒ Idle" defaults listed in r1-followups)

**Interfaces:**
- Produces, in `command.rs`. These are exactly spec §4.1, plus the drop types from §4.3:

```rust
pub trait CommandLanguage: Send + Sync {
    fn split(&self, text: &str) -> Vec<CommandSpan>;
    fn parse(&self, text: &str) -> Result<ParsedCommand, CommandParseError>;
    fn redact(&self, text: &str) -> Redacted;
}
pub struct CommandSpan { pub range: (usize, usize), pub text: String }
pub struct ParsedCommand {
    pub name: String,
    pub class: CommandClass,
    pub targets: Vec<CommandTarget>,
    pub unbounded: bool,
    pub drops: Vec<CommandDrop>,
    pub session_effect: Option<CommandSessionEffect>,
    pub payload: CommandPayload,
}
pub enum CommandClass { Read, Write, Ddl, Admin, SessionChange, Transaction(TxControl), Unknown }
pub struct CommandTarget { pub database: Option<String>, pub name: String }
pub struct CommandDrop { pub kind: CommandDropKind, pub target: CommandTarget }
pub enum CommandDropKind { Object, Database }
pub enum CommandSessionEffect { UseDatabase(String) }
pub type CommandPayload = Arc<dyn Any + Send + Sync>;
pub struct CommandParseError { pub message: String, pub offset: Option<usize> }

#[async_trait]
pub trait CommandSession: Session {
    fn language(&self) -> &dyn CommandLanguage;
    fn tx_state(&self) -> TxState;
    fn data_access(&self) -> &dyn DataAccess;   // trait from Task 4: in this task, declare `pub trait DataAccess: Send + Sync {}` in core/src/data_access.rs; Task 4 fills it
    async fn execute_command(&self, cmd: &ParsedCommand, opts: &ExecOptions,
                             sink: &mut dyn RowSink) -> Result<StmtOutcome, DbError>;
}
```

- `ParsedCommand` derives `Clone, Debug`; `Debug` for `payload` prints `<payload>`. `CommandClass`, `CommandTarget`, `CommandDrop`, `CommandDropKind` and `CommandSessionEffect` derive `Debug, Clone, PartialEq, Eq`.
- `crates/core/src/redact.rs` holds `pub struct Redacted { pub text: String, pub changed: bool }`, moved from `dnt_query::redact`, which now does `pub use dnt_core::redact::Redacted`.
- In `driver.rs`:
  - `Engine::Command(&'a dyn CommandSession)`.
  - `dyn Session::sql()` returns `Err(DbError::Unsupported { feature: format!("SQL on {}", self.server_info().kind.label()), version: String::new() })` for `Command`.
  - New `dyn Session::command() -> Result<&dyn CommandSession, DbError>`. For `Sql` it returns `Unsupported { feature: "commands on <label>" }`.
  - New `dyn Session::tx_state() -> TxState`, which asks whichever engine the session has.
- The "no SqlSession ⇒ Idle" sites in `query_sessions.rs` (`tx_open`, `tx_info`, `check_ddl_idle`, `tx_now`) and the data-session reuse checks use `dyn Session::tx_state()`. A Command session in a transaction is then never seen as idle.

- [ ] **Step 1: Write failing tests** in `driver.rs` tests. Add a minimal `FakeCommand` session (not the testkit one) whose `tx_state` is `Active`. Then:

```rust
#[test]
fn engines_answer_for_themselves() {
    let c: Box<dyn Session> = Box::new(fake_command(TxState::Active));
    assert!(matches!(c.sql(), Err(DbError::Unsupported { feature, .. }) if feature == "SQL on PostgreSQL"));
    assert!(c.command().is_ok());
    assert_eq!(c.tx_state(), TxState::Active);
    let s: Box<dyn Session> = Box::new(fake());
    assert!(matches!(s.command(), Err(DbError::Unsupported { .. })));
    assert_eq!(s.tx_state(), TxState::Idle);
}
```

  The test fake reports `DbKind::Postgres`; the label check is the point.
- [ ] **Step 2: Run** `cargo test -p dnt-core engines_answer_for_themselves`. Expected: compile error.
- [ ] **Step 3: Implement.** Move `Redacted`, then replace the Idle defaults.
- [ ] **Step 4: Run** `cargo test --workspace`, `kind_branches` and `bindings_are_current`. Expected: PASS, with `bindings.ts` unchanged.
- [ ] **Step 5: Commit** `feat(core): Command engine and command-language contract`.

### Task 3: Metadata for Command engines

**Files:**
- Modify: `crates/core/src/meta.rs` (`ObjectKind`)
- Create: `crates/core/src/keyspace.rs`
- Modify: `crates/core/src/lib.rs`, `crates/core/src/driver.rs` (`Session::keyspace`)
- Modify: every Rust `match` on `ObjectKind` that the compiler names
- Modify: `ui/src/bindings.ts` (regenerated)
- Modify: the six UI `Record<ObjectKind, …>` maps (`openDdlEdit.ts:11`, `TabBar.tsx:43`, `TreeContextMenu.tsx:78`, `ObjectActionDialog.tsx:28`, `treeModel.ts:107`, `DdlEditorTab.tsx:13`) and any other place `tsc` names

**Interfaces:**
- Produces:
  - `ObjectKind::Collection` and `ObjectKind::Key`.
  - In `keyspace.rs`, exactly the types of spec §5.2: `Keyspace` (with `scan` and `key_value`), `ScanRequest`, `KeyPage`, `KeySummary`, `KeyName { display, raw }`, `KeyType`, `ValueRange`, `KeyValue`, `StreamEntry { id: String, fields: Vec<(Value, Value)> }`.
  - `KeyValue` carries `ttl_ms: Option<i64>` and `memory_bytes: Option<u64>` next to the typed value. Use a struct `KeyValueView { value: KeyValue, ttl_ms, memory_bytes }` as `key_value`'s return type.
  - `KeyName` serializes `raw` as a standard-alphabet base64 string, via `#[serde(with = …)]` over the workspace `base64` crate. Add `base64.workspace = true` to `dnt-core`'s Cargo.toml; this is a workspace dependency, not a new crate.
  - `KeyName::from_bytes(raw: Vec<u8>) -> KeyName`: `display` is the lossy UTF-8 text with every invalid byte shown as `\xNN`.
  - `Session::keyspace(&self) -> Option<&dyn Keyspace>`: a default method returning `None`.
- Rust matches on `ObjectKind`: SQL code that cannot meet `Collection`/`Key` gets an explicit arm doing exactly what that function does for unsupported kinds today. For example, `ddl.rs`'s "outside P4a" refusal, or the `ObjectKind::Type`-like path. List every one in the report.
- UI maps: `Collection` and `Key` get neutral entries, for example the icon or label of `Table`, `'Collection'` / `'Key'` labels, and empty editable lists. List them.

- [ ] **Step 1: Write failing tests** in `keyspace.rs`:

```rust
#[test]
fn key_name_shows_invalid_bytes() {
    let k = KeyName::from_bytes(vec![b'a', 0xff, b'b']);
    assert_eq!(k.display, "a\\xffb");
    assert_eq!(serde_json::to_value(&k).unwrap()["raw"], "Yf9i");
    let back: KeyName = serde_json::from_value(serde_json::to_value(&k).unwrap()).unwrap();
    assert_eq!(back, k);
}
```

- [ ] **Step 2: Run** `cargo test -p dnt-core key_name_shows_invalid_bytes`. Expected: compile error.
- [ ] **Step 3: Implement**, fix the matches, regenerate bindings and fix the UI maps.
- [ ] **Step 4: Run** `cargo test --workspace`, `kind_branches`, `bindings_are_current`, `pnpm --dir ui lint` and `pnpm --dir ui test`. Expected: PASS. `git diff ui/src/bindings.ts` shows only the two `ObjectKind` variants.
- [ ] **Step 5: Commit** `feat(core): Collection/Key object kinds and the Keyspace API`.

### Task 4: Edit data types in core, `DataAccess`

**Files:**
- Create: `crates/core/src/edit.rs`. Move `CellInput`, `RowRef`, `RowEdit` and `EditSet` from `crates/query/src/edit.rs`; leave `pub use dnt_core::edit::{CellInput, RowEdit, RowRef, EditSet};` there.
- Modify: `crates/core/src/data_access.rs` (Task 2 stub)

**Interfaces:**
- Produces, spec §7.4:

```rust
pub trait DataAccess: Send + Sync {
    fn page(&self, obj: &ObjectRef, spec: &PageSpec) -> Result<ParsedCommand, DbError>;
    fn count(&self, obj: &ObjectRef, filter: Option<&DataFilter>) -> Result<ParsedCommand, DbError>;
    fn build_edits(&self, obj: &ObjectRef, detail: &TableDetail, edits: &EditSet) -> Result<Vec<ParsedCommand>, DbError>;
    fn build_key_edit(&self, db: &str, edit: &KeyEdit) -> Result<Vec<ParsedCommand>, DbError>;
}
pub struct PageSpec { pub sort: Vec<SortSpec>, pub filter: Option<DataFilter>, pub limit: u32, pub skip: u64 }
pub enum KeyEdit {
    SetString { key: KeyName, value: String },
    HashSet { key: KeyName, field: String, value: String },
    HashDelete { key: KeyName, field: String },
    ListPush { key: KeyName, value: String, head: bool },
    ListSet { key: KeyName, index: i64, value: String },
    ListRemove { key: KeyName, value: String, count: i64 },
    SetAdd { key: KeyName, member: String },
    SetRemove { key: KeyName, member: String },
    ZSetAdd { key: KeyName, member: String, score: f64 },
    ZSetRemove { key: KeyName, member: String },
    Expire { key: KeyName, ttl_ms: Option<i64> },   // None = persist
    Rename { key: KeyName, to: KeyName },
    Delete { key: KeyName },
}
pub fn check_read(cmd: &ParsedCommand) -> Result<(), DbError>;
// Err(DbError::Internal { message: format!("the driver built `{}` for a read, but it is not a read", cmd.name) }) unless cmd.class == CommandClass::Read
```

- `DataFilter` and `SortSpec` stay in `dnt_core::sql::filter`. Import them; do not move them.
- `KeyEdit` derives `Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type` with `#[serde(tag = "op", rename_all = "snake_case")]`. It is not exported to bindings in R2.

- [ ] **Step 1: Write failing tests** in `data_access.rs`: `check_read_refuses_writes`. It builds a `ParsedCommand` with class `Write` and expects `Err(Internal)` whose message names the command; class `Read` gives `Ok`.
- [ ] **Step 2: Run** `cargo test -p dnt-core check_read_refuses_writes`. Expected: FAIL.
- [ ] **Step 3: Implement**, moving the four types with their doc comments unchanged. `uuid` is already a core dependency.
- [ ] **Step 4: Run** `cargo test --workspace` and `bindings_are_current`. Expected: PASS, with `bindings.ts` unchanged by the move.
- [ ] **Step 5: Commit** `feat(core): DataAccess and edit data types in core`.

### Task 5: `query::policy`

**Files:**
- Create: `crates/query/src/policy.rs`
- Create: `crates/query/tests/policy_matrix.rs`
- Modify: `crates/query/src/lib.rs`

**Interfaces:**
- Consumes: `CommandDrop` / `CommandDropKind` (Task 2), `Verdict` (`crate::plan`), `EnvTag`.
- Produces, exactly spec §4.3: `RiskClass`, `RiskFacts { class, unbounded, drops: Vec<CommandDrop> }`, `PolicyCtx { env, read_only, confirm_unbounded, connection_name }`, `PolicyOutcome { verdict, confirm_name }`, and `pub fn policy(facts: &RiskFacts, ctx: &PolicyCtx) -> PolicyOutcome`. Also `impl From<CommandClass> for RiskClass`, where `Transaction(_)` maps to `Transaction`.
- Rules: copy the spec §4.3 table and its risk rules verbatim. For `confirm_name`:
  - only on Production with verdict Confirm;
  - `unbounded`, or any drop, gives the first target's `name`;
  - `Unknown` with no target gives `ctx.connection_name`;
  - otherwise `None`.

- [ ] **Step 1: Write failing tests.**
  - Unit tests in `policy.rs`, one per table row and risk rule:
    - `read_allowed_everywhere`;
    - `write_blocked_read_only_confirm_production_allow_staging_local`;
    - `unbounded_confirms_on_staging_and_local_when_set`;
    - `drop_object_confirms_on_staging`;
    - `drop_database_confirms_everywhere`;
    - `unknown_needs_connection_name_on_production`.
  - `policy_matrix_matches_sql_guard` in `crates/query/tests/policy_matrix.rs`:
    - **Statement → `RiskFacts` mapping:**
      - `SELECT 1` → Read;
      - `BEGIN` → Transaction;
      - `UPDATE t SET a = 1 WHERE id = 1` → Write;
      - `UPDATE t SET a = 1` → Write, unbounded, target `t`;
      - `CREATE TABLE t (a int)` → Ddl;
      - `DROP TABLE t` → Ddl, drop Object `t`;
      - `DROP DATABASE d` → Ddl, drop Database `d`;
      - `GRANT SELECT ON t TO u` → Admin.
    - **Procedure:** for each pair, both dialects, each env (`Production`, `Staging`, `Local`, `None`), each `read_only` and each `confirm_unbounded`, run `plan(...)` then `guard_with(.., GuardCtx { env, read_only, analyze: false, confirm_unbounded })`.
    - **Assert:** stmt 0's `(verdict, confirm_name)` equals `policy(&facts, &ctx)`.
    - **On mismatch:** fix `policy`, never the SQL guard. If a mismatch is a real spec/guard disagreement, stop and report it (BLOCKED) with the failing cell.
- [ ] **Step 2: Run** `cargo test -p dnt-query policy`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p dnt-query`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(query): shared guard policy`.

### Task 6: `query::command` — plan and guard

**Files:**
- Create: `crates/query/src/command.rs`
- Modify: `crates/query/src/lib.rs`, `crates/query/src/plan.rs` (`StmtKindLabel::Admin`)
- Modify: UI places `tsc` names for the new label (`RunConfirmDialog.tsx` and others). The label renders as `Admin`.

**Interfaces:**
- Consumes: Task 2 (`CommandLanguage`, `ParsedCommand`, `CommandParseError`), Task 5 (`policy`, `PolicyCtx`, `RiskFacts`), `Target`, `Verdict`, `Utf16Map`.
- Produces:

```rust
pub struct PlannedCommand {
    pub index: u32,
    pub range: (u32, u32),            // UTF-16, editor text
    pub source_range: (usize, usize), // bytes, editor text
    pub text: String,
    pub parsed: Result<ParsedCommand, CommandParseError>,
    pub label: StmtKindLabel,
    pub verdict: Verdict,
    pub confirm_name: Option<String>,
}
pub struct CommandPlan { pub source: String, pub base: u32, pub stmts: Vec<PlannedCommand>, pub guard: Option<PolicyCtx> }
pub fn plan_commands(text: &str, target: Target, lang: &dyn CommandLanguage) -> CommandPlan;
pub fn guard_commands(plan: &mut CommandPlan, ctx: &PolicyCtx);
impl CommandPlan { pub fn runnable(&self) -> bool }   // every stmt parsed and none Block
pub fn label_of(class: &CommandClass) -> StmtKindLabel;
```

- `Target` semantics are the same as `plan::plan`: Cursor gives the span containing the cursor (both ends inclusive), else the nearest one ending before it, else the first. Selection splits only the slice. Empty or whitespace-only input gives an empty plan.
- `label_of` mapping:

  | `CommandClass` | `StmtKindLabel` |
  |---|---|
  | Read | Read |
  | Write | Write |
  | Ddl | Ddl |
  | Transaction | Transaction |
  | SessionChange | Other |
  | Admin, Unknown | Admin |

  A parse error gets label `Other`.
- `guard_commands` sets every stmt's verdict and `confirm_name` from `policy`, using `RiskFacts::from(&ParsedCommand)`. A parse error is `Verdict::Block` with `confirm_name: None`.

- [ ] **Step 1: Write failing tests** in `command.rs`. Use a tiny in-file test language with one command per line; `GET` is Read, `SET` Write, `NUKE` Unknown, and anything with `!` is a parse error.
  - `cursor_picks_the_line`
  - `selection_plans_only_the_slice`
  - `ranges_are_utf16`: for `SET k "héllo😀"\nGET k`, stmt 1's `range.0` is the UTF-16 offset of `GET`.
  - `parse_error_blocks_the_whole_script`: `GET a\nSET !\nGET b` gives stmt 1 Block and `runnable() == false`.
  - `unknown_on_production_needs_connection_name`
  - `read_only_blocks_writes`
- [ ] **Step 2: Run** `cargo test -p dnt-query command::`. Expected: FAIL.
- [ ] **Step 3: Implement.** Add `StmtKindLabel::Admin` (serde `"admin"`), regenerate bindings and render it in the UI.
- [ ] **Step 4: Run** `cargo test --workspace`, `bindings_are_current`, `pnpm --dir ui lint` and `pnpm --dir ui test`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(query): plan and guard command scripts`.

### Task 7: `query::command` — run

**Files:**
- Modify: `crates/query/src/command.rs`

**Interfaces:**
- Consumes: Task 6; `RunConfig`, `RunCancel`, `RunReport`, `StmtReport`, `StopReason`, `reconnect_notice` (`crate::run`); `CommandSession` (Task 2); `AppError` mapping as in `run.rs`.
- Produces:

```rust
pub trait CommandRunSink: RowSink {
    fn statement_start(&mut self, stmt: &PlannedCommand);
    fn statement_end(&mut self, stmt: &PlannedCommand, result: &Result<StmtOutcome, AppError>);
    fn notice(&mut self, index: u32, message: &str);
    fn tx_state(&mut self, state: TxState);
    fn database(&mut self, name: &str);   // a SessionChange took effect
}
pub async fn run_commands(session: &dyn CommandSession, plan: &CommandPlan, opts: &ExecOptions,
                          cfg: RunConfig, cancel: &RunCancel, sink: &mut dyn CommandRunSink) -> RunReport;
```

- Rules:
  - A plan that is not `runnable()` sends nothing. Its report has `stop: Some(StopReason::Error { index: <first blocked> })` and an empty `stmts`.
  - Every command gets `opts` with `stop` replaced by `cancel.signal()`.
  - Stop before a command gives `StopReason::Cancelled`, and that command is not sent.
  - The run stops at the first error unless `cfg.continue_on_error` is set; a lost connection or `outcome_unknown` always stops it.
  - After each command, a changed `session.tx_state()` is reported through `sink.tx_state`, and a `SessionChange` that succeeded is reported through `sink.database`.
  - Reconnects are counted as `run_with` counts them and reported with `reconnect_notice`.
  - `cfg.manual_tx` is ignored (spec §4.4).

- [ ] **Step 1: Write failing tests** with an in-file fake `CommandSession` that records what it receives:
  - `blocked_plan_sends_nothing`
  - `stops_at_first_error`
  - `continue_on_error_goes_on`
  - `cancel_before_second_stops`: the sink raises the cancel in `statement_end` of stmt 0; stmt 1 is never received.
  - `tx_state_changes_are_reported`
- [ ] **Step 2: Run** `cargo test -p dnt-query command::run`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p dnt-query`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(query): run command scripts`.

### Task 8: `FakeCommandDriver` (testkit) and end-to-end tests

**Files:**
- Create: `crates/testkit/src/fake_command.rs`
- Modify: `crates/testkit/src/lib.rs`, `crates/testkit/Cargo.toml` (only workspace deps already used by core: `async-trait`, `tokio`, `serde_json`)
- Test: `crates/query/tests/command_e2e.rs`

**Interfaces:**
- Consumes: Tasks 2–7.
- Produces:
  - `pub struct FakeCommandDriver` with `fn new() -> Self` and `fn store(&self) -> FakeStore`. `FakeStore` is a cloneable shared handle used to inspect data and set hooks.
  - It implements `Driver`. `kind()` returns `DbKind::Postgres`, so that R2 app tests can store its connection; the doc comment says so.
  - `ServerInfo::new(DbKind::Postgres, ServerVariant::Postgres, (0, 0, 0), "fake-command")` with `capabilities = Capabilities { family: Command, namespaces: NumberedDb, data_edit: true, paged_objects: true, everything else false }`.
  - The session implements `Session`, `CommandSession`, `Introspector`, `Keyspace` and `DataAccess`.
  - **Language:**
    - **Splitting:** one command per non-empty line; a line starting with `#` is a comment.
    - **Tokens:** separated by whitespace; `"…"` is a quoted token with `\"` and `\\` escapes.
    - **Command table:**
      - `GET k`, `HGETALL k`, `SCAN cursor [MATCH p]` → Read;
      - `SET k v`, `DEL k…`, `HSET k f v` → Write;
      - `SELECT n` → SessionChange, effect `UseDatabase(n)`;
      - `MULTI` / `EXEC` / `DISCARD` → Transaction Begin / Commit / Rollback;
      - `FLUSHDB` → Admin, drop `Database` (the current db);
      - `CONFIG …` → Admin;
      - anything else → Unknown;
      - a wrong number of arguments or an unclosed quote → `CommandParseError` with offset.
    - **Redaction:** `CONFIG SET requirepass <x>` becomes `CONFIG SET requirepass ***`.
  - **Data:** dbs 0–15 of `BTreeMap<Vec<u8>, FakeValue>`, where `FakeValue` is `Str(String)` or `Hash(BTreeMap<String, String>)`.
  - **Behaviour:**
    - `execute_command` writes rows through the sink: GET gives one column `value`; HGETALL gives `field`, `value`.
    - Inside `MULTI`, commands are queued and run on `EXEC`.
    - `opts.stop` raised before a command starts → `Cancelled { outcome_unknown: false, was_in_tx }`.
    - A hook `store.block_next()` makes the next command wait until cancelled, for the cancel test.
  - `Keyspace::scan` pages through sorted keys with a numeric string cursor. `DataAccess::build_key_edit` supports `SetString`, `HashSet`, `HashDelete` and `Delete`; the others → `Unsupported`.

- [ ] **Step 1: Write failing tests** in `crates/query/tests/command_e2e.rs`, all through `plan_commands`, `guard_commands` and `run_commands` against the fake:
  - `set_then_get_roundtrip`
  - `select_changes_db_for_the_rest`: `SET a 1\nSELECT 2\nGET a` gives Null in db 2; the sink saw `database("2")`.
  - `stop_before_start_sends_nothing`
  - `cancel_stops_a_blocked_command`: `store.block_next()`, then the run is spawned; `RunCancel::cancel()` makes that command end `Cancelled`, and the run stops with `StopReason::Cancelled`.
  - `open_multi_reports_active`: `MULTI\nSET a 1`, then `session.tx_state() == Active` and `a` is not in the store.
  - `exec_commits_queued`
  - `flushdb_on_production_needs_db_name`: `confirm_name == Some("0")`.
  - `unknown_command_blocked_read_only`
  - `scan_pages_keys`
  - `key_edit_builds_write_commands`: `build_key_edit` gives a Write `SET`; `check_read` rejects it.
  - `redact_hides_requirepass`
- [ ] **Step 2: Run** `cargo test -p dnt-query --test command_e2e`. Expected: FAIL.
- [ ] **Step 3: Implement** the fake.
- [ ] **Step 4: Run** `cargo test -p dnt-query --test command_e2e` and `cargo test -p dnt-testkit`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(testkit): in-memory Command driver`.

### Task 9: App-side Command-session and gate tests

**Files:**
- Test: `src-tauri/tests/command_sessions.rs` (new)
- Modify: production code only when a test exposes a real defect; each such fix is listed in the report.

**Interfaces:**
- Consumes: `FakeCommandDriver` (Task 8), registered for `DbKind::Postgres` in a test `SessionManager`, using the same construction as `src-tauri/tests/session_manager.rs`. Also the `require_cap` / `require_sql` gates from R1.

- [ ] **Step 1: Write the tests.** A Postgres connection whose driver returns a Command session:
  - `sql_paths_refuse_a_command_session`. Each of these returns `DbError::Unsupported` whose feature is `"SQL on PostgreSQL"`, and no session leaks: `close` was called on every opened session (the fake counts opens and closes):
    - `table_data_impl`
    - `query_plan` then `query_run` (the `query_run_named` path)
    - `table_design_open_impl`
    - `import_target`
    - `dump_objects`
  - `tab_with_command_session_is_not_idle`: after the fake runs `MULTI` through its session, `tx_info` / `check_ddl_idle` see it as Active.
  - `every_cap_gate_refuses_when_off`. Table-driven: for each `Cap` in `Cap::ALL`, a SQL fake with that capability off makes one representative `_impl` return `Unsupported { feature: cap.label() }`.

    | `Cap` | Representative `_impl` |
    |---|---|
    | Designer | `table_design_open` |
    | DdlEdit | `ddl_edit_open` |
    | ObjectActions | `object_action_preview` |
    | Import | `import_target` |
    | Dump | `dump_objects` |
    | Seed | `seed_target` |
    | Explain | `query_explain` |
    | DataEdit | `data_editable` |
- [ ] **Step 2: Run** `cargo test -p dntdatabase --test command_sessions`. Expected: PASS, or FAIL revealing a real defect. Fix only that defect and record RED/GREEN.
- [ ] **Step 3: Commit** `test(app): Command sessions and capability gates`.

### Task 10: Full verification

- [ ] **Step 1: Run the CI set.** Expected: every command passes.

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
pnpm --dir ui lint
pnpm --dir ui test
```

- [ ] **Step 2: Run the DB-backed suite** against the already-running `dnt-test` stack, without compose up/down, because that stack is the user's. Use `DNT_TEST_DB=1` and `cargo test --workspace -- --test-threads=1`. Expected: PASS, apart from the known cancel-race flakes listed in r1-followups (report them if they recur). No `.snap.new` files.
- [ ] **Step 3: Run e2e-web:** `$env:E2E_WEB_PORT='5176'; pnpm --dir e2e-web test`. Expected: PASS.
- [ ] **Step 4: Commit** only if a step required a change.
