# Redis driver Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A real Redis driver (`crates/driver-redis`), registered in `default_drivers()`, so that Redis connections work end to end with the R4a backend and R4b UI.

**Architecture:**
- **Client:** a small in-house RESP2/RESP3 client, one connection per session, on tokio + tokio-rustls (ring).
- **Classification:** the console language classifies commands from the server's own `COMMAND INFO` flags, plus a small hand-written exception list. Its static form, available before connect, only splits and redacts.
- **Keyspace:** browsing uses `SCAN` / `HSCAN` / `SSCAN`. Key edits never overwrite an existing key (`SET NX`, `RENAMENX`, small `EVAL` scripts).
- **Core changes:** the shared contracts change in three places (`ValueRange.cursor`, `KeyEdit::Create`, `ListRemove { index, value }`). The fake driver and the UI follow these changes first.

**Tech Stack:** Rust 1.99, tokio, tokio-rustls 0.26 (ring, no default features), rustls 0.23 (ring), async-trait, secrecy, base64. React/TypeScript for the small UI change. Docker Compose for the Redis 6.2 and 8 test services.

**Spec:** `docs/superpowers/specs/2026-11-05-redis-driver-design.md`. Read it with the core spec `2026-10-31-non-sql-driver-core-design.md` and the hand-off `2026-11-04-r4b-followups.md`.

## Global Constraints

- **MySQL/PostgreSQL unchanged.** Their behaviour stays the same and every existing test passes. Insta snapshots do not change.
- **TLS:** rustls with ring only. No OpenSSL and no aws-lc-rs. After adding `tokio-rustls`, `Cargo.lock` must contain none of `aws-lc-rs`, `aws-lc-sys`, `openssl-sys` or `native-tls`.
- **Dependencies:** no third-party Redis crate. The only new dependency is `tokio-rustls` (workspace dependency, `default-features = false`, features `ring`, `tls12`).
- **Copy:** identifiers, error text and UI copy are English, and the exact strings below are used verbatim.
- **Safety promises** (spec §1): no command reaches the server except through a guarded plan; read-only connections never receive writes (the driver checks this as well, spec §5.2); no automatic retry; secrets never reach History, logs or Favorites.
- **Kind branches:** Rust uses exhaustive `match` only, and the `kind_branches` test stays green. In the UI, no kind literals appear outside `ui/src/engine/`.
- **Bindings:** regenerate with `cargo run -p dntdatabase --example export_bindings`, then `bindings_are_current` must pass. Bindings change only in Task 1.
- **Verification per task:** run `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, and the task's tests. A task that touches the UI also runs `pnpm --dir ui lint` and `pnpm --dir ui test`. DB-backed tests run with `DNT_TEST_DB=1` against the compose stack, using `--test-threads=1`.
- **Files:** UTF-8 without BOM (`no_bom` test). Do not write files with PowerShell `Set-Content -Encoding utf8`.
- **Commits:** style `feat(core|redis|app|ui|testkit): …`, one commit per task, each ending with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Review Focus

1. **The connection drops while `SELECT 2` is in effect.** The next command reports `ConnectionLost`, and the one after it runs in db 2, never in db 0. Pinned by Task 6 `reconnect_restores_selected_db`.
2. **A write typed on a read-only connection that somehow bypassed the guard.** The driver refuses it and the server sees nothing. Pinned by Task 6 `read_only_refuses_writes_in_driver`.
3. **`AUTH`, `CONFIG SET requirepass` or `ACL SETUSER u >pw` in the console, including a line with an unterminated quote.** The secret never appears in the redacted text. Pinned by Task 4 `redacts_secrets_even_unparsed`.
4. **"New key" on a name that already exists, of any type.** The old value is unchanged and the user sees `Key already exists`. Pinned by Task 9 `create_refuses_existing_key`.
5. **"Remove item 3" on a list with duplicate values, or a list another client changed after the preview.** Only item 3 is removed, or nothing is removed with `The list changed; refresh and try again`. Pinned by Task 9 `list_remove_by_index`.

---

### Task 1: Core contract changes, fake driver, bindings

**Files:**
- Modify: `crates/core/src/keyspace.rs` (`ValueRange.cursor`)
- Modify: `crates/core/src/data_access.rs` (`KeyEdit::Create`, `NewKeyValue`, `ListRemove` shape)
- Modify: `crates/core/src/error.rs` (`static_hint` for Redis codes)
- Modify: `crates/testkit/src/fake_command.rs` (new shapes; hash paging by cursor)
- Modify: `src-tauri/src/edit_plans.rs` and `src-tauri/src/commands/keys.rs`, wherever these types are matched
- Modify: `ui/src/bindings.ts` (regenerated), plus the minimal TS changes needed to compile (`ui/src/tabs/keys/keyEdits.ts`, `ui/src/store/keysTabs.ts`, `e2e-web/src/backend.ts` or `command*.ts`)

**Interfaces:**
- Produces:

```rust
pub struct ValueRange { pub offset: u64, pub count: u32, #[serde(default)] pub cursor: Option<String> }
pub enum NewKeyValue {                       // serde tag "type", snake_case
    String { value: KeyBytes },
    Hash { field: KeyBytes, value: KeyBytes },
    List { value: KeyBytes },
    Set { member: KeyBytes },
    ZSet { member: KeyBytes, score: f64 },
}
// KeyEdit (tag "op"): adds
Create { key: KeyName, value: NewKeyValue },   // op "create"
// and ListRemove becomes
ListRemove { key: KeyName, index: i64, value: KeyBytes },
```

- `static_hint` gains entries for `DbError::Server` codes:

| Code | Hint |
|---|---|
| `NOPERM` | `The ACL user lacks permission for this command` |
| `READONLY` | `This server is a read-only replica` |
| `LOADING` | `The server is loading data or running a script; try again later` |
| `BUSY` | `The server is loading data or running a script; try again later` |

- **Fake driver:**
  - `Create` works like the real driver: it is refused with `Server` "Key already exists" when the key exists.
  - `ListRemove` removes by index when the item at that index equals `value`. Otherwise it fails with "The list changed; refresh and try again".
  - Hash `key_value` pages by `cursor` (a stringified index) and returns `next`.
- **UI, minimal compile only** (Task 2 owns behaviour): build `ListRemove` with `index` and `value`, and send `cursor: null`.

- [ ] **Step 1: Write the failing tests.**
  - core: `value_range_cursor_defaults_none`, which deserialises `{"offset":0,"count":10}`.
  - core: `key_edit_create_serde`, a round trip of `{"op":"create","key":…,"value":{"type":"hash","field":…,"value":…}}`.
  - core: `hint_for_redis_codes`, covering the four codes above.
  - testkit: `fake_create_refuses_existing`, `fake_list_remove_by_index` (duplicates, changed list), and `fake_hash_pages_by_cursor`.
- [ ] **Step 2: Run** `cargo test -p dnt-core -p dnt-testkit`. Expected: FAIL (compile errors / assertions).
- [ ] **Step 3: Implement**, then regenerate the bindings. The bindings may change only in `ValueRange`, `KeyEdit` and `NewKeyValue`. Fix the TS to compile.
- [ ] **Step 4: Run** `cargo test --workspace`, `bindings_are_current`, `pnpm --dir ui lint`, `pnpm --dir ui test` and `$env:E2E_WEB_PORT='5176'; pnpm --dir e2e-web test specs/key-browser.spec.ts`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(core): key edits create without overwrite, list remove by index, value cursors`.

### Task 2: Key browser UI follows the new contracts

**Files:**
- Modify: `ui/src/tabs/keys/keyEdits.ts`, `ui/src/tabs/keys/KeyDialogs.tsx` ("New key…"), `ui/src/tabs/keys/KeyValuePane.tsx` ("Remove item"), `ui/src/store/keysTabs.ts` (cursor paging)
- Modify: `e2e-web/src/` key-browser fake (Create, ListRemove by index, hash/set cursor)
- Test: `ui/src/tabs/keys/KeysTab.test.tsx`, `ui/src/store/keysTabs.test.ts`, `e2e-web/specs/key-browser.spec.ts`

**Interfaces:**
- Consumes: the Task 1 bindings.
- **New key:** "New key…" sends `{ op: 'create', key, value: NewKeyValue }` for all five types (it used to send `set_string`, `hash_set`, …).
- **Remove item:** "Remove item N" on a list sends `{ op: 'list_remove', key, index: N, value: <exact bytes of item N> }`. The control stays hidden when the bytes are not exact, as today.
- **Load more:** for hash and set, "Load more" sends `range.cursor = <previous KeyValue.next>`. `next === null` hides "Load more" for these two types. List and zset keep offset paging; stream uses the cursor as its last id.
- **Errors:** `Key already exists` and `The list changed; refresh and try again` appear in the preview dialog's error banner, which is the existing path.

- [ ] **Step 1: Write the failing tests:**
  - `new_key_sends_create` (all five types)
  - `remove_item_sends_index_and_value`
  - `hash_load_more_uses_cursor`
  - `set_without_next_has_no_load_more`
  - `create_conflict_shows_message`
- [ ] **Step 2: Run** `pnpm --dir ui test keys`. Expected: FAIL.
- [ ] **Step 3: Implement.** Update the e2e fake to the same semantics.
- [ ] **Step 4: Run** lint, `pnpm --dir ui test`, and the e2e `key-browser.spec.ts`. Extend the spec so that New key on an existing name shows `Key already exists`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(ui): key browser creates without overwrite and pages hash/set by cursor`.

### Task 3: Crate skeleton and RESP codec

**Files:**
- Create: `crates/driver-redis/Cargo.toml`, `crates/driver-redis/src/lib.rs` (module declarations only), `crates/driver-redis/src/resp.rs`
- Modify: root `Cargo.toml` (`tokio-rustls` workspace dependency)

**Interfaces:**
- Produces (`resp.rs`):

```rust
pub enum Frame {
    Simple(Vec<u8>), Error(Vec<u8>), Integer(i64), Bulk(Vec<u8>), Null,
    Array(Vec<Frame>), Set(Vec<Frame>), Push(Vec<Frame>), Map(Vec<(Frame, Frame)>),
    Double(f64), Boolean(bool), BigNumber(String), Verbatim { format: String, text: Vec<u8> },
}
pub fn encode(argv: &[Vec<u8>], out: &mut Vec<u8>);                       // RESP array of bulk strings
pub fn decode(buf: &[u8]) -> Result<Option<(Frame, usize)>, RespError>;  // None: need more bytes
pub struct RespError(pub String);
```

- **Decoding rules:**
  - RESP2 null bulk (`$-1`) and null array (`*-1`) both decode to `Null`.
  - RESP3 `_` decodes to `Null`.
  - Blob error `!` decodes to `Error`.
  - An attribute frame `|` is read and dropped; `decode` returns the frame that follows it.
  - Nesting deeper than 512 levels gives `RespError("nesting too deep")`.
  - A bulk length above 512 MiB gives `RespError("bulk too large")`.
- `Cargo.toml` lists only workspace dependencies (`dnt-core`, `tokio` with `net`, `io-util`, `time`, `sync`, `macros`, `rt`; `tokio-rustls`, `rustls`, `rustls-platform-verifier`, `async-trait`, `secrecy`, `base64`, `tracing`, `rand`). If one is missing from the workspace, add it the same way as `tokio-rustls`.

- [ ] **Step 1: Write the failing tests** in `resp.rs`:
  - `round_trips_every_frame`
  - `decodes_incrementally_at_every_split`: for each sample, split the bytes at every index and assert `Ok(None)` until the full length.
  - `resp2_nulls_are_null`
  - `skips_attributes`
  - `rejects_deep_nesting`
  - `encodes_binary_argv`
- [ ] **Step 2: Run** `cargo test -p dnt-driver-redis resp`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the tests. Also run `cargo tree -i aws-lc-rs` and `cargo tree -i openssl-sys`. Expected: tests PASS, and both trees report the package as not found.
- [ ] **Step 5: Commit** `feat(redis): crate skeleton and RESP2/RESP3 codec`.

### Task 4: Console language: lexer, split, classify, redact

**Files:**
- Create: `crates/driver-redis/src/language/{mod.rs,lexer.rs,classify.rs}`

**Interfaces:**
- Produces:

```rust
pub fn split_args(line: &[u8]) -> Result<Vec<Vec<u8>>, LexError>;   // sdssplitargs port
pub struct LexError { pub message: &'static str, pub offset: usize }
pub struct CmdFlags { pub readonly: bool, pub write: bool, pub admin: bool, pub dangerous: bool }
pub struct RedisLanguage { flags: Option<Arc<HashMap<String, CmdFlags>>> }  // None = static form
impl RedisLanguage {
    pub fn static_form() -> Self;
    pub fn from_command_info(reply: &Frame) -> Self;   // keys "get", "config|set", lowercase
}
impl CommandLanguage for RedisLanguage { /* split, parse, redact */ }
pub(crate) struct Payload { pub argv: Vec<Vec<u8>>, pub blocking: bool, pub human: bool }
```

- **Lexer** (spec §4.2):
  - The error messages are exactly `Unterminated quotes` and `Closing quote must be followed by a space`.
  - Escapes inside `"…"`: `\xHH`, `\n`, `\r`, `\t`, `\b`, `\a`. Any other escaped character is kept as written.
  - Inside `'…'`, the only escape is `\'`.
- **Split** (spec §4.1):
  - Lines whose first non-space character is `#` are comments.
  - A line that starts with whitespace joins the previous command.
  - Ranges are byte offsets into the text, and each range spans every joined line.
- **Parse** (spec §4.3):
  - The exception table is checked first, verbatim from the spec.
  - Then container subcommands are looked up as `name|sub`.
  - Then flags are applied: `admin` or `dangerous` → `Admin`, `write` → `Write`, `readonly` → `Read`, no flag → `Read`. A name missing from the table → `Unknown`.
  - In the static form, every name outside the exceptions is `Unknown`.
  - `SELECT` with a non-numeric or negative argument is a parse error: `ERR invalid DB index`.
- **Command fields:**
  - `ParsedCommand.text` is the command as written.
  - `targets` and `drops` follow the spec exceptions: `FLUSHDB` is a `Database` drop with an empty name and `database: None`; `FLUSHALL` is `AllDatabases`.
  - `unbounded` is always false.
- **Payload flags:**
  - `blocking` is true for the twelve commands in spec §5.3.
  - `human` is true for the commands in spec §6, matched as a case-insensitive prefix on the joined words.
- **Redact:** follows spec §4.4. On a lex error it falls back to whitespace words, so a password on an unterminated line is still masked.

- [ ] **Step 1: Write the failing tests:**
  - `lexer_matches_sdssplitargs` (a table of at least 20 cases, binary included)
  - `lexer_errors`
  - `split_lines_comments_and_continuations`
  - `classifies_by_flags` (a synthetic `COMMAND INFO` frame with `get`, `set`, `config|set`, `ping`)
  - `exceptions_win`
  - `unsupported_commands_are_parse_errors` (every name in the spec list)
  - `static_form_is_unknown_outside_exceptions`
  - `flush_drops`
  - `redacts_secrets_even_unparsed` (every pattern in spec §4.4, plus `AUTH "pw` unterminated)
  - `blocking_and_human_flags`
- [ ] **Step 2: Run** `cargo test -p dnt-driver-redis language`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the tests. Expected: PASS.
- [ ] **Step 5: Commit** `feat(redis): console language (split, classify, redact)`.

### Task 5: Connection, TLS, handshake, test services

**Files:**
- Create: `crates/driver-redis/src/conn.rs`, `crates/driver-redis/src/tls.rs`
- Modify: `crates/driver-redis/src/lib.rs` (`RedisDriver`, `Driver::connect` returning a minimal session from Task 6's type; until then, the tests here use `conn::open` directly)
- Modify: `testdata/docker-compose.test.yml` (services `redis62` and `redis8`)
- Create: `testdata/redis/redis8.conf` and `testdata/redis/users.acl`
- Modify: `crates/testkit/src/lib.rs` (`Target::Redis62`, `Target::Redis8`, ports, `resolved_as` for Redis)
- Test: `crates/driver-redis/tests/connect.rs`

**Interfaces:**
- Produces:

```rust
pub(crate) struct Conn { /* stream: TCP or TLS, read buffer */ }
pub(crate) struct Handshake { pub client_id: i64, pub version: (u32, u32, u32), pub version_string: String }
pub(crate) async fn open(cfg: &ResolvedConfig) -> Result<(Conn, Handshake, Frame /* COMMAND INFO or Null */), DbError>;
impl Conn {
    pub async fn call(&mut self, argv: &[Vec<u8>]) -> Result<Frame, DbError>;           // IO error -> ConnectionLost
    pub async fn pipeline(&mut self, cmds: &[Vec<Vec<u8>>]) -> Result<Vec<Frame>, DbError>;
    pub async fn select(&mut self, db: u16) -> Result<(), DbError>;
}
pub(crate) fn connector(ssl: Option<&SslConfig>, server_name: &str) -> Result<Option<TlsConnector>, DbError>;
```

- **Handshake and errors:** follow spec §5.1 and §8 exactly. Each case maps as follows:

| Case | Result |
|---|---|
| `HELLO` is an unknown command | `Unsupported { feature: "Redis 6.0 or later is required", version: "" }` |
| `WRONGPASS` or `NOAUTH` | `Auth` |
| `NOPERM` | `Auth` |
| `redis_mode:cluster` | `Unsupported` "Redis Cluster is not supported yet" |
| bad db | `Config { field: Some("options.driver.db"), message: "Database <n> does not exist on this server" }` |
| `COMMAND INFO` denied | returns `Frame::Null` and logs one warning |

- **TLS:** the verifiers follow `driver-pg/src/tls.rs`: `NoVerification` for `Unverified`, a chain-only verifier for `VerifyCa`, and webpki with `tls_server_name` for `VerifyFull`. Client certificates are taken from `cert_path` and `key_path`.
- **Compose:**
  - `redis62`: `redis:6.2-alpine`, port `63062`.
  - `redis8`: `redis:8` (or the official 8.x tag), port `63080`, TLS port `63443` with the certs in `./certs`, an ACL file, and `enable-debug-command local`.
  - Both services set `enable-debug-command local`.
  - `users.acl` defines `default` with password `dnt_test` and user `dnt` with password `dnt_test` and `~* +@all -flushall -config -debug -command`.
- **testkit:**
  - `Target::Redis62` and `Target::Redis8`; `port()` returns the ports above, and `tls_port()` is `Some(63443)` only for Redis 8.
  - `resolved_as` gives `DriverOptions::Redis { db: 0 }`, user `default`, and password `dnt_test`.

- [ ] **Step 1: Write the failing tests** in `tests/connect.rs` (all `require_db!()`):
  - `connects_plain_both_versions`
  - `connects_tls_verify_full` and `connects_tls_unverified`
  - `acl_user_connects` and `wrong_password_is_auth`
  - `bad_db_is_config_error`
  - `command_info_denied_returns_null` (user `dnt`)
  - Unit test in `tls.rs`: `prefer_and_require_are_unverified`
- [ ] **Step 2: Run** the compose stack and `cargo test -p dnt-driver-redis --test connect` with `DNT_TEST_DB=1`. Expected: FAIL.
- [ ] **Step 3: Implement.** Document the new services in the compose header comment.
- [ ] **Step 4: Run** the tests again, plus `cargo test -p dnt-testkit`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(redis): connection, TLS and handshake; Redis 6.2 and 8 test services`.

### Task 6: Session, command execution and reply mapping

**Files:**
- Create: `crates/driver-redis/src/session.rs`, `crates/driver-redis/src/reply.rs`
- Modify: `crates/driver-redis/src/lib.rs` (`Driver::connect` returns `RedisSession`; `command_language()` returns `RedisLanguage::static_form()`)
- Test: `crates/driver-redis/tests/session.rs`; unit tests in `reply.rs`

**Interfaces:**
- Produces:

```rust
pub struct RedisSession { /* Mutex<Option<Conn>>, cfg, info, language, db, tx, reconnects, canceller */ }
impl Session for RedisSession { /* server_info, engine -> Engine::Command(self), introspector, keyspace -> Some(self), cancel_handle, reconnects, ping, close */ }
impl CommandSession for RedisSession { /* language, tx_state, current_database, data_access, execute_command */ }
pub(crate) fn write_reply(cmd: &ParsedCommand, payload: &Payload, frame: Frame, opts: &ExecOptions,
                          sink: &mut dyn RowSink, started: Instant) -> Result<ResultSummary, DbError>;
```

- **`execute_command` steps:**
  1. Downcast the payload to `Payload`; a wrong type is `Internal`.
  2. On a read-only connection, refuse every class except `Read`, `SessionChange` and `Transaction` with `Config { field: None, message: "This connection is read-only" }`, before taking the connection lock.
  3. Reconnect if the connection is gone (spec §5.2).
  4. Send the command, honouring `opts.timeout` and `opts.stop`.
  5. Map the reply as in spec §6.
  6. Update `db` after a successful `SELECT`, and `tx` on `MULTI`, `EXEC`, `DISCARD` and `EXECABORT`.
- **Inside `MULTI`:** each queued command produces one result with one row, `value` = `QUEUED`. `EXEC` produces one result per queued reply, in order.
- **Errors:**
  - An error reply maps to `Server { code: <first word>, sqlstate: None, message, position: None }`.
  - `MOVED` and `ASK` map to `Unsupported` "Redis Cluster is not supported yet".
  - An `EXECABORT` reply maps to `Server` with the message `Transaction aborted: <msg>`.
- **Lost connection:**
  - The error is `ConnectionLost { was_in_tx, outcome_unknown: class != Read, message }`.
  - The connection is dropped, and the next call reconnects, re-selects `db` and increments `reconnects()`.
  - A transaction that was open is discarded, the state goes back to `Idle`, and the sink receives the notice `The connection was lost; the open transaction was discarded`.
- **`affected`:** a `Write` command that replies with an integer sets `affected`.

- [ ] **Step 1: Write the failing tests:**
  - Reply unit tests: `scalar_reply`, `array_reply`, `map_reply`, `nil_reply`, `non_utf8_is_bytes`, `nested_is_json`, `write_integer_sets_affected`, `human_reply_splits_lines`, `max_rows_truncates`.
  - Integration tests (both versions):
    - `get_set_round_trip`
    - `hgetall_is_field_value`
    - `select_updates_current_database`
    - `multi_exec_results`
    - `execabort_resets_tx`
    - `read_only_refuses_writes_in_driver` (`DBSIZE` unchanged)
    - `reconnect_restores_selected_db` (kill our client id from a side connection with `CLIENT KILL ID`)
    - `language_classifies_json_set_on_redis8`
    - `session_language_unknown_without_command_info` (user `dnt`)
- [ ] **Step 2: Run** them. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p dnt-driver-redis` with `DNT_TEST_DB=1`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(redis): session and command execution`.

### Task 7: Cancel

**Files:**
- Create: `crates/driver-redis/src/cancel.rs`
- Modify: `crates/driver-redis/src/session.rs` (record the running command; drop path)
- Test: `crates/driver-redis/tests/cancel.rs`

**Interfaces:**
- Produces `pub(crate) struct RedisCanceller` (implements `Canceller`). It holds the connection settings, `client_id` and an `Arc<Mutex<Option<Running { blocking: bool, class: CommandClass }>>>`, plus the session's drop signal.
- **Blocking commands** (`Running.blocking` true):
  1. Raise `opts.stop`.
  2. Open a side connection with `conn::open` under a 5 s timeout.
  3. Send `CLIENT UNBLOCK <id> ERROR`.
  4. The main command returns `Cancelled { outcome_unknown: false, was_in_tx }`, and the connection stays usable.
- **Any other command, or when the side connection fails, or `cancel_grace` expires:**
  - Drop the main connection.
  - The command returns `Cancelled { outcome_unknown: class != Read, was_in_tx }`.
  - The next call reconnects.

- [ ] **Step 1: Write the failing tests:**
  - `cancel_blpop_unblocks`: run `BLPOP k 0`, cancel after 200 ms. Expect `Cancelled`, then the same session's `PING` succeeds and `reconnects() == 0`.
  - `cancel_debug_sleep_drops`: run `DEBUG SLEEP 5`, cancel. Expect `Cancelled` within about 1 s, then the next command succeeds and `reconnects() == 1`.
  - `cancel_idle_is_noop`
- [ ] **Step 2: Run** them. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p dnt-driver-redis --test cancel` with `DNT_TEST_DB=1`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(redis): cancel by CLIENT UNBLOCK or connection drop`.

### Task 8: Introspector and keyspace

**Files:**
- Create: `crates/driver-redis/src/introspect.rs`, `crates/driver-redis/src/keyspace.rs`
- Test: `crates/driver-redis/tests/keyspace.rs`

**Interfaces:**
- `Introspector`:
  - `list_databases` reads `INFO keyspace`, adds the current db, sorts by number, and sets `key_count`.
  - `list_schemas`, `list_objects` and `list_columns` return `[]`.
  - `table_detail` and `object_ddl` return `Unsupported`.
- `Keyspace::scan` and `Keyspace::key_value` follow spec §7.2 and §7.3 exactly:
  - the `KeyType` mapping, including `ReJSON-RL` → `Json`;
  - a key whose type is `none` is dropped from a scan page;
  - `KeyNotFound` leaves the connection clean;
  - an error from `MEMORY USAGE` gives `memory_bytes: None`;
  - `GETRANGE` is capped at `ExecOptions::default()`'s cell limit;
  - `HSCAN` and `SSCAN` page by cursor, and `XRANGE` uses `cursor` as an exclusive start id.
- The `db` argument selects the database. Both methods `SELECT` it when it differs from the session db, then switch back, holding the lock for the whole call.

- [ ] **Step 1: Write the failing tests:**
  - `list_databases_counts_keys`
  - `scan_pattern_and_type_to_the_end`
  - `key_value_each_type` (string, hash, list, set, zset, stream; JSON on Redis 8)
  - `hash_and_set_page_by_cursor` (500 fields, count 100)
  - `stream_pages_by_last_id`
  - `missing_key_is_key_not_found` (then `PING` works)
  - `memory_usage_denied_is_none` (user `dnt` with `-memory` added to `users.acl`)
  - `large_string_is_preview`
- [ ] **Step 2: Run** them. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p dnt-driver-redis --test keyspace` with `DNT_TEST_DB=1`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(redis): introspector and keyspace`.

### Task 9: Key edits (`DataAccess`)

**Files:**
- Create: `crates/driver-redis/src/edits.rs`
- Test: unit tests in `edits.rs`; `crates/driver-redis/tests/edits.rs`

**Interfaces:**
- `build_key_edit` follows the mapping table in spec §7.4 exactly:
  - The scripts and argv layout come from spec §7.5.
  - The sentinel is 16 random bytes from `rand`.
  - `targets = [{ database: Some(db), name: key.display }]`.
  - The class is `Write`.
  - `text` is the redis-cli rendering, with quoting from `KeyName.display`. The text is display-only and never executed.
- `page`, `count` and `build_edits` return `Unsupported { feature: "collections", version: "" }`.
- **Reply handling in `execute_command`** (`Payload` gains `on_nil: Option<&'static str>` and `on_zero: Option<&'static str>`):
  - A `SET … NX` that replies nil fails with `Server` "Key already exists".
  - A `RENAMENX` that replies `0` fails with `Server` "Key already exists".
  - Errors raised by the scripts surface as `Server` with the script's message.

- [ ] **Step 1: Write the failing tests:**
  - Unit test `argv_for_every_edit`: every `KeyEdit` variant gives the exact argv and text.
  - Integration tests:
    - `create_each_type`
    - `create_refuses_existing_key` (each type, old value intact)
    - `renamenx_refuses_existing`
    - `list_remove_by_index` (duplicates removed exactly once at the index; changed list → error, nothing removed)
    - `set_string_keeps_ttl`
    - `expire_and_persist`
    - `delete_unlinks`
- [ ] **Step 2: Run** them. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p dnt-driver-redis` with `DNT_TEST_DB=1`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(redis): key edits without overwrite`.

### Task 10: Register the driver, app tests, full verification, hand-off

**Files:**
- Modify: `src-tauri/Cargo.toml` (depend on `dnt-driver-redis`), `src-tauri/src/state.rs` (`default_drivers()` adds `RedisDriver`)
- Test: `src-tauri/tests/redis_driver.rs` (new)
- Create: `docs/superpowers/plans/2026-11-05-redis-driver-followups.md`

**Interfaces:**
- `default_drivers()` registers MySQL, Postgres and Redis.

- [ ] **Step 1: Write the failing tests:**
  - `available_engines_include_redis`
  - `language_for_redis_is_static_form`: it splits `GET a\nSET b 1` into 2 spans and redacts `AUTH x`.
  - `redis_end_to_end` (`require_db!`): through `AppState` with a saved Redis 8 connection:
    - open it and list databases;
    - plan and run `SET r4 1` then `GET r4`;
    - `keys_scan` finds `r4`;
    - `key_edit_preview` + `data_apply` for `Delete`;
    - History has redacted rows for `AUTH`.
  - `redis_through_ssh_tunnel` (`require_db!`): the same saved connection with SSH through the compose `ssh` service (target host `redis8` inside the compose network, the way the existing SSH tests reach their DB) opens and runs `PING`.
- [ ] **Step 2: Run** them. Expected: FAIL.
- [ ] **Step 3: Implement** the registration.
- [ ] **Step 4: Run the full CI set:**
  - `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` (with and without `DNT_TEST_DB=1` and `--test-threads=1`);
  - `pnpm --dir ui lint`, `pnpm --dir ui test`, `pnpm --dir e2e-web typecheck`, `$env:E2E_WEB_PORT='5176'; pnpm --dir e2e-web test`;
  - `cargo tree -i aws-lc-rs` reports not found.

  Expected: PASS. A smoke check in the real app is optional; record it in the hand-off.
- [ ] **Step 5: Write the hand-off doc.** It covers decisions carried forward, anything the reviews found, and what Mongo can reuse. Commit `feat(app): register the Redis driver` and `docs: Redis driver hand-off`.
