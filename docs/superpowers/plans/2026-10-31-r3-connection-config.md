# R3 — Connection config for Command engines Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Mongo and Redis connections can be described, validated, parsed from a URI and saved. They cannot be opened yet: no driver is registered, so opening fails with "no driver registered for Mongo/Redis".

**Architecture:** Driver-specific options are stored as `DriverOptions` inside the existing `ConnOptions` (`options_json`).
- `ConnectionConfig::validate` checks per kind.
- `conn_string` parses `mongodb://`, `mongodb+srv://`, `redis://` and `rediss://`. The password is returned separately and never stored in the URI, and `to_uri` prints a URI without it.
- A pure `command_tls` helper maps `SslConfig` for Command drivers.
- Store migration 10 rebuilds `connections` to accept the new kinds. It runs with foreign keys OFF, so child rows (history, jobs, tabs) survive.

**Tech Stack:** Rust 1.99 stable, serde/specta/tauri-specta, rusqlite + rusqlite_migration, React 19 + TS (bindings and one dialog guard).

**Spec:** `docs/superpowers/specs/2026-10-31-non-sql-driver-core-design.md`, §6.1–6.5, R3 in §8. Read `docs/superpowers/plans/2026-10-31-r2-followups.md` too. §6.6 (dialog fields) and the engine picker are R4.

## Global Constraints

- **No behaviour change for MySQL/PostgreSQL connections.** Their validation, parsing, storage and import stay byte-identical. Every existing test passes, except the R1 tests that pinned "Mongo/Redis connections are not supported yet"; those change to the new rules.
- **Secrets never go to SQLite, logs, `Debug` or JSON** (P1 rule). A password parsed from a URI is returned in the existing `ParsedConnString.password` field only, and `to_uri` never prints it.
- **No new crate or npm package.**
- **Identifiers, error text and UI copy are in English.** Exact strings below are verbatim.
- **Bindings:** run `cargo run -p dntdatabase --example export_bindings`. `cargo test -p dntdatabase --test commands bindings_are_current` must pass. `ui/src/bindings.ts` may gain only the following:
  - `DriverOptions` and `HostPort`;
  - `ConnOptions.driver`;
  - the `ConnStringEngine` variants `mongo`, `mongo_srv`, `redis` and `rediss`, written however serde names them;
  - `ParsedConnString.driver`.
- **`kind_branches` stays green.**
- **Verification per task:** `cargo check --workspace --all-targets`, the task's own tests, `kind_branches`, `cargo fmt --all`, and `cargo clippy --workspace --all-targets -- -D warnings`. If bindings change, also run `pnpm --dir ui lint` and `pnpm --dir ui test`.
- **Commit style:** `feat(core|store|app): …`, one commit per task.
- **Decisions this plan makes:**
  1. `ConnStringEngine` gains `Mongo`, `MongoSrv`, `Redis` and `Rediss`. `rediss://` maps to `ssl.mode = Require`.
  2. Unknown URI options are an error for Mongo/Redis (spec §6.3: `Unsupported URI option: <name>`). MySQL/PG keep their current "ignored + warning" behaviour.
  3. Migration 10 runs with foreign keys off. `prepare()` enables foreign keys after `to_latest`, and M10 carries `.foreign_key_check()`.
  4. Until R4, the connection dialog refuses to apply a pasted Mongo/Redis URI. It shows `MongoDB and Redis connections are not available in this version yet.` and does not silently map the URI to MySQL.

## Review Focus

1. **Upgrading a real database.** Every `query_history` row, every `jobs.connection_id` and every tab link survives migration 10, and foreign keys are on afterwards. Pinned by Task 3 test `migration_10_keeps_child_rows`.
2. **A Mongo URI password with reserved characters** (`p%40ss%2Fw`) decodes to `p@ss/w` and appears in neither `to_uri`, `Debug` nor the JSON of `ParsedConnString` minus `password`. Pinned by Task 4 test `mongo_password_is_extracted_and_never_printed`.
3. **`mongodb+srv://` with a port, or with several hosts,** is rejected. SRV forbids both. Pinned by Task 4 test `srv_rejects_port_and_host_list`.
4. **`options_json` saved by older versions** (`'{}'` and full objects without `driver`) parses to `DriverOptions::None`. Pinned by Task 1 test `old_options_json_has_no_driver_options`.
5. **A Mongo connection with SSH and extra hosts or SRV** fails validation on field `ssh` with `SSH tunnel needs a single host`. Pinned by Task 2 test `ssh_needs_a_single_host`.

---

### Task 1: `DriverOptions` in the config model

**Files:**
- Modify: `crates/core/src/config.rs`
- Modify: callers that build `ConnOptions` / `ResolvedConfig` literals (the compiler names them); `crates/core/src/test_util.rs`
- Modify: `ui/src/bindings.ts` (regenerated) and the TS literals `tsc` names, so that every UI `ConnOptions` literal gets `driver: { kind: "none" }`, or whatever the serde tag gives

**Interfaces:**
- Produces, in `config.rs`. These are exactly spec §6.1:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct HostPort { pub host: String, pub port: u16 }

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DriverOptions {
    #[default]
    None,
    Mongo { srv: bool, auth_source: Option<String>, replica_set: Option<String>,
            direct_connection: bool, extra_hosts: Vec<HostPort> },
    Redis { db: u16 },
}
// ConnOptions gains (struct already has #[serde(default)]):
pub driver: DriverOptions,
// ResolvedConfig gains:
pub driver: DriverOptions,
```

- Wherever a `ResolvedConfig` is built from a `ConnectionConfig` (session_manager's resolve path), set `driver: cfg.options.driver.clone()`.

- [ ] **Step 1: Write the failing tests** in `config.rs`:

```rust
#[test]
fn old_options_json_has_no_driver_options() {
    let o: ConnOptions = serde_json::from_str("{}").unwrap();
    assert_eq!(o.driver, DriverOptions::None);
    let o: ConnOptions = serde_json::from_str(r#"{"version":1,"connect_timeout_s":10,"ssh_timeout_s":10,"charset":null,"application_name":null,"query_timeout_s":0}"#).unwrap();
    assert_eq!(o.driver, DriverOptions::None);
}
#[test]
fn driver_options_serde_shape() {
    let r = DriverOptions::Redis { db: 3 };
    assert_eq!(serde_json::to_value(&r).unwrap(), serde_json::json!({"kind":"redis","db":3}));
    let m: DriverOptions = serde_json::from_value(serde_json::json!({"kind":"mongo","srv":true,"auth_source":"admin","replica_set":null,"direct_connection":false,"extra_hosts":[]})).unwrap();
    assert!(matches!(m, DriverOptions::Mongo { srv: true, .. }));
}
```

- [ ] **Step 2: Run** `cargo test -p dnt-core old_options_json driver_options_serde`. Expected: compile error.
- [ ] **Step 3: Implement.** Regenerate the bindings and fix the TS literals.
- [ ] **Step 4: Run** `cargo test --workspace`, `bindings_are_current`, `pnpm --dir ui lint` and `pnpm --dir ui test`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(core): driver-specific connection options`.

### Task 2: Validation per kind, and the TLS mapping

**Files:**
- Modify: `crates/core/src/config.rs`
- Create: `crates/core/src/tls.rs`
- Modify: `crates/core/src/lib.rs`
- Modify: the R1 tests that pinned "not supported yet": `config.rs` `validate_rejects_unsupported_kinds` and the store's `import_rejects_unsupported_kind`

**Interfaces:**
- Produces:
  - `ConnectionConfig::validate` becomes an exhaustive `match self.kind`. MySQL and PG keep today's body (`validate_sql`) unchanged. Mongo and Redis follow spec §6.2:
    - **Shared checks:**
      - `name` must not be empty, with today's message.
      - `options.driver` must match the kind, otherwise `Config { field: "options.driver", message: "Driver options do not match the engine" }`.
      - The existing ssl `verify_*` requires a CA, with today's message.
    - **Mongo:**
      - `host` is required, with today's message.
      - `port` is required unless `srv`, with today's message.
      - `username` is optional.
      - `srv` together with a non-empty `extra_hosts` gives `Config { field: "options.driver", message: "An SRV connection string cannot list extra hosts" }`.
      - `ssh` together with `srv` or a non-empty `extra_hosts` gives `Config { field: "ssh", message: "SSH tunnel needs a single host" }`.
      - Each extra host needs a non-empty host and a port other than 0, otherwise `field: "options.driver.extra_hosts"`, message `Every additional host needs a host and a port`.
    - **Redis:**
      - `host` and `port` are required, with today's messages.
      - `username` is optional.
      - `db` is not checked here, because the server reports a db index out of range.
  - In `tls.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandTls { Off, Unverified, VerifyCa, VerifyFull }
/// Spec §6.4: how a Command driver (Mongo/Redis) applies `SslConfig`. Those servers have no
/// "prefer": `Prefer` and `Require` both mean TLS without certificate checks.
pub fn command_tls(ssl: Option<&SslConfig>) -> CommandTls;
// None | Disable → Off; Prefer | Require → Unverified; VerifyCa → VerifyCa; VerifyFull → VerifyFull
```

- [ ] **Step 1: Write the failing tests:**
  - `mongo_rules`: covers port 0 with `srv`, port 0 without `srv`, an empty username, and mismatched driver options.
  - `redis_rules`
  - `ssh_needs_a_single_host`: Mongo with `srv` plus SSH, and Mongo with `extra_hosts` plus SSH. Both give field `ssh`; check the exact message.
  - `srv_with_extra_hosts_is_refused`
  - `extra_host_needs_host_and_port`
  - `mysql_and_pg_rules_unchanged`: the existing `validate_checks_every_field` cases still pass.
  - `command_tls_maps_every_mode`
  - Replace `validate_rejects_unsupported_kinds` with `mongo_and_redis_validate_now`.
- [ ] **Step 2: Run** `cargo test -p dnt-core config:: tls::`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Update the store's `import_rejects_unsupported_kind`.** A valid Mongo `.dntconn` entry no longer fails validation; it fails on the SQLite CHECK until Task 3. Make the test assert the Task 3 outcome, and mark it `#[ignore = "enabled in Task 3"]` if that cannot pass yet. Task 3 removes the ignore. Then run `cargo test --workspace`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(core): validate Mongo/Redis connections; TLS mapping for Command drivers`.

### Task 3: Store migration 10, plus saving a Mongo/Redis connection

**Files:**
- Modify: `crates/store/src/db.rs` (`MIGRATION_10`, `migrations()`, `prepare()`)
- Modify: `crates/store/src/repo.rs` if needed (it already maps "mongo"/"redis")
- Test: `crates/store/src/db.rs` tests, `crates/store/tests/` and `src-tauri/tests/session_manager.rs` (or a new `src-tauri/tests/command_connections.rs`)

**Interfaces:**
- Produces:
  - `MIGRATION_10` rebuilds `connections` with `CHECK (kind IN ('mysql','postgres','mongo','redis'))`. Columns, defaults, column order, the `groups` FK and every index stay as they are today; copy them from the current schema, meaning the last migration that touched `connections`.
  - The pattern is `CREATE TABLE connections_next …; INSERT INTO connections_next SELECT * FROM connections; DROP TABLE connections; ALTER TABLE connections_next RENAME TO connections;`, followed by any indexes.
  - The migration is registered as `M::up(MIGRATION_10).foreign_key_check()`.
- Why `prepare()` must change: today it enables `foreign_keys` before `to_latest`. With foreign keys on, `DROP TABLE connections` runs an implicit `DELETE` that cascades into `query_history` and the other children, and SET NULLs `jobs.connection_id`.
- The required order in `prepare()`:
  1. `pragma_update(foreign_keys, false)`;
  2. `to_latest`;
  3. `pragma_update(foreign_keys, true)`;
  4. verify that `PRAGMA foreign_keys` reads 1.

  The existing test asserting that foreign keys are on after `open` must keep passing.

- [ ] **Step 1: Write the failing tests.**
  - `migration_10_keeps_child_rows`:
    - Build a database at version 9 with `Migrations::new(vec![M::up(MIGRATION_1) … M::up(MIGRATION_9)])` on an in-memory connection with foreign keys ON (as today).
    - Insert:
      - 2 connections;
      - 3 `query_history` rows for connection A;
      - 1 `jobs` row with `connection_id` = A;
      - one row in every other table that references `connections(id)`. Grep `REFERENCES connections` in db.rs.
    - Run `prepare()`'s migration path, which goes through the public `open_in_memory`-style helper. Expose a `pub(crate) fn migrate(conn)` used by both paths if needed.
    - Assert:
      - all rows are still present, and `jobs.connection_id` is still A;
      - `PRAGMA foreign_key_check` is empty;
      - `PRAGMA foreign_keys` is 1;
      - inserting a connection with kind `'mongo'` and one with `'redis'` now succeeds;
      - kind `'oracle'` still fails.
  - `repo_round_trips_mongo_and_redis`: `ConnectionRepo` create/get of a Mongo config with `DriverOptions::Mongo { … }` and a Redis config, field for field.
  - **App-level:** creating a Redis connection through `create_connection_impl` (or the repo plus `SessionManager`) succeeds. Opening it fails with `DbError::Internal { message }` where `message == "no driver registered for Redis"`.
  - Un-ignore `import_rejects_unsupported_kind` and rename it to `import_accepts_mongo`: the Mongo entry is imported.
- [ ] **Step 2: Run** `cargo test -p dnt-store` and `cargo test -p dntdatabase --test <file>`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --workspace`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(store): connections accept mongo and redis (migration 10)`.

### Task 4: Mongo/Redis URIs, and the dialog guard

**Files:**
- Modify: `crates/core/src/conn_string.rs`
- Modify: `ui/src/bindings.ts` (regenerated)
- Modify: `ui/src/dialogs/ConnectionDialog.tsx`, where a parsed engine is applied (around line 315: `p.engine === 'postgres' ? 'postgres' : 'mysql'`), and its test

**Interfaces:**
- Produces:
  - `ConnStringEngine` gains `Mongo`, `MongoSrv`, `Redis` and `Rediss`.
  - `ParsedConnString` gains `driver: Option<DriverOptions>`.
  - New `pub fn to_uri(cfg: &ConnectionConfig) -> Result<String, DbError>` builds a URI without a password. Use percent-encoding for the user and any other component. `mongodb+srv://` when `srv`. `rediss://` when ssl mode ≥ Require. MySQL/PG return a `postgresql://` / `mysql://` URI in the same shape.
- Parsing rules, spec §6.3:
  - **Schemes:** `mongodb` gives `Mongo`, `mongodb+srv` gives `MongoSrv` with `driver.srv = true`, `redis` gives `Redis`, `rediss` gives `Rediss` with `ssl_mode = Some(SslMode::Require)`. The unsupported-scheme message lists all of them.
  - **Mongo hosts:**
    - `host1:p1,host2:p2`: the first becomes `host`/`port` and the rest `extra_hosts`.
    - A missing port is 27017, except under SRV, where it is not allowed.
    - SRV with a port gives `Config { field: "uri", message: "mongodb+srv:// does not allow a port" }`. SRV with several hosts gives `"mongodb+srv:// takes a single host name"`.
  - **Mongo `/db`:** `database`.
  - **Mongo query options:**
    - `authSource` gives `auth_source`; `replicaSet` gives `replica_set`.
    - `directConnection=true|false` sets that flag.
    - `tls` / `ssl` `=true` gives `ssl_mode = Require`; `false` gives `Disable`.
    - `tlsCAFile` is refused with `Unsupported URI option: tlsCAFile (pick the CA file in the dialog)`.
    - `appName` gives `application_name`; `connectTimeoutMS` gives `connect_timeout_s`, rounded up.
    - Anything else: `Config { field: "uri", message: "Unsupported URI option: <name>" }`.
  - **Redis:**
    - `redis://[user][:password]@host[:port][/db]`: port defaults to 6379, `/n` sets `driver.db = n`, the default db is 0.
    - A db that is not a number, or is over 65535, gives `Config { field: "uri", message: "Redis database must be a number" }`.
    - Every query option is unsupported (same error), except `timeout`, which sets `connect_timeout_s`.
  - **Passwords:** percent-decoded into `password`, as for SQL. Never kept anywhere else.
- **UI:** when the parsed engine is any of the four new engines, the dialog does not change the form. It shows the inline error `MongoDB and Redis connections are not available in this version yet.` R4 replaces this.

- [ ] **Step 1: Write the failing tests** in `conn_string.rs`:
  - `mongo_password_is_extracted_and_never_printed`: `mongodb://app:p%40ss%2Fw@h1:27018,h2/sales?authSource=admin&replicaSet=rs0` gives:
    - password `p@ss/w`;
    - host `h1`, port 27018;
    - `extra_hosts == [h2:27017]`;
    - db `sales`;
    - `auth_source` admin, `replica_set` rs0.

    In addition, `format!("{:?}", parsed)` contains no `p@ss/w` (redact `password` in `Debug` if it is not already redacted), and `to_uri` of a config built from it is `mongodb://app@h1:27018,h2:27017/sales?authSource=admin&replicaSet=rs0`.
  - `srv_rejects_port_and_host_list`
  - `srv_sets_the_flag_and_no_port`: `mongodb+srv://u@cluster0.x.mongodb.net/db` gives srv, port `None`, host set.
  - `mongo_unknown_option_is_an_error`
  - `rediss_requires_tls_and_reads_db`: `rediss://default:pw@cache:6380/2` gives `Rediss`, Require, port 6380, db 2, user `default`, password `pw`.
  - `redis_defaults`: `redis://cache` gives port 6379 and db 0.
  - `redis_bad_db`
  - `to_uri_never_contains_the_password`: for each engine.
  - `sql_parsing_unchanged`: every existing test passes untouched.
  - A UI vitest in `ConnectionDialog.test.tsx`: pasting `redis://cache` shows the exact message and leaves the engine and host fields unchanged.
- [ ] **Step 2: Run** `cargo test -p dnt-core conn_string::`. Expected: FAIL.
- [ ] **Step 3: Implement.** Regenerate the bindings and add the dialog guard.
- [ ] **Step 4: Run** `cargo test --workspace`, `bindings_are_current`, `pnpm --dir ui lint` and `pnpm --dir ui test`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(core): parse Mongo/Redis connection strings`.

### Task 5: `.dntconn` and `.ncx`

**Files:**
- Modify: `crates/store/src/transfer.rs`, `crates/store/src/ncx.rs` (only if a test exposes a gap)
- Test: `crates/store/tests/` (existing ncx/transfer test files)

**Interfaces:**
- **`.dntconn` export and import** carry `kind` and `options.driver`; this is serde-driven, so no format change. A file from an older version still imports.
- **`.ncx`:** Navicat `MONGODB` and `REDIS` entries are listed in `skipped` with the reason `<name>: MongoDB connections are not imported from Navicat yet` / `<name>: Redis connections are not imported from Navicat yet`. Use the existing `SkippedItem` shape and check its fields. Other unknown types keep today's reason.

- [ ] **Step 1: Write the failing tests:**
  - `dntconn_round_trips_mongo_and_redis`: export, then import, gives equal configs, `options.driver` included.
  - `dntconn_from_older_version_still_imports`: a stored fixture JSON without `driver` gives `DriverOptions::None`.
  - `ncx_skips_mongo_and_redis_with_reason`: build an ncx snippet with ConnType `MONGODB` and `REDIS`, using the same XML shape as the existing ncx tests.
- [ ] **Step 2: Run** `cargo test -p dnt-store`. Expected: FAIL for the ncx reason; the round trip may already pass, so note that in the report.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p dnt-store`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(store): Mongo/Redis in .dntconn; named skips in .ncx`.

### Task 6: Full verification

- [ ] **Step 1: Run the CI set:** `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `pnpm --dir ui lint` and `pnpm --dir ui test`. Expected: PASS.
- [ ] **Step 2: Run the DB suite** against the user's running `dnt-test` stack, without compose up/down. Copy in that stack's certs/keys from `D:\DntDatabase\testdata`, then restore this worktree's own, verifying by hash. Run with `DNT_TEST_DB=1` and `--test-threads=1`. Expected: PASS, apart from the known MySQL cancel flakes, which are fixed on unmerged branches.
- [ ] **Step 3: Run e2e-web:** `$env:E2E_WEB_PORT='5176'; pnpm --dir e2e-web test`. Expected: PASS.
- [ ] **Step 4: Upgrade check on a copy of the user's real database.** Copy `%APPDATA%\DntDatabase\app.db` to a temp dir, if it exists; never open the original. Run a small test binary or `cargo test` helper that opens the copy through `dnt_store::db::open`. Report the row counts before and after for `connections`, `query_history` and `jobs`; they must be equal. Delete the copy afterwards.
- [ ] **Step 5: Commit** only if a step required a change.
