# P1 core: final whole-branch review (c445904..3722264, feat/p1-core, 36 commits)

Reviewer passes (read-only; working tree verified clean at HEAD 3722264, so files were read in place instead of the 1.5 MB diff package):

1. Docs: spec (all sections, §10 in detail), `deferred-and-rulings.md`, `global-constraints.md`, the commit list and `--stat`.
2. Core: `config.rs`, `secret.rs`, `error.rs`, `meta.rs`, `value.rs`.
3. App shell: `session_manager.rs`, `cache.rs`, `commands/{mod,connections,browse,transfer,settings}.rs`, `state.rs`, `main.rs`, `logging.rs`, `build.rs`, `tauri.conf.json`, `capabilities/default.json`, `Cargo.toml`; test inventories of `src-tauri/tests/*`.
4. Tunnel: `lib.rs`, `known_hosts.rs`, `auth.rs`; test inventory.
5. Drivers: mysql `lib.rs`, `tls.rs`, `queries.rs`, session (grep); pg `lib.rs`, `introspect.rs` (`conn_for`, `table_detail`); connect-test inventories.
6. Store: `repo.rs` (tree and CRUD), `dntconn.rs` (excerpts), `transfer.rs` (apply excerpt), `testdata/ncx/sample.ncx`.
7. UI: `api/{errors,index}.ts`, `store/{tree,treeModel,objectData,tabs}.ts`, `ObjectTab`, `DialogHost`, `ConnectionDialog`, `ImportDialog`, `ExportDialog` (grep), `PasswordPrompt`, `HostKeyDialog`, `ConnectionTree`, `TreeContextMenu`, `StatusBar`, `TabBar`, `Sidebar`, `App`, `ErrorBoundary`; UI test fixtures (grep).
8. Build, CI and E2E: `ci.yml`, `e2e/wdio.conf.ts`, `e2e/smoke.e2e.ts`, root `package.json`, README (grep), and a BOM scan of every tracked file.

Lockfiles, snapshot bodies, icons and `ui/src/bindings.ts` were skimmed only. No tests were run.

---

### Strengths

- **Layering is clean and close to the spec.** `core` holds types and traits only. Drivers do not depend on Tauri or SQLite. Commands are thin `*_impl` functions, so they can be tested without a Tauri runtime. `guard`/`blocking` wrap every command and drop the panic payload without reading it (`commands/mod.rs:67-98`).
- **Secret hygiene is good on the main paths.** IPC input types have no `Debug`. Values become `SecretString` right away. `ResolvedConfig` redacts its `Debug` output. The tree returns only presence flags. Key-file and passphrase errors never echo the secret. `.dntconn` decryption drops serde errors on purpose (`dntconn.rs:343`). A test proves that command output contains no secrets (`tests/commands.rs:30`).
- **SessionManager lifecycle is careful.** Generation-gated install happens under one lock. `PendingGuard` handles dropped futures. The cache epoch stops a stale result from being cached across a refresh. A dead tunnel is reopened once. Metadata calls and shutdown are bounded in time. The fake-driver tests cover the races (`tests/session_manager.rs:555-700`).
- **The NodeKey contract matches across layers.** Backend `folder_key`/`object_key` (`browse.rs:89-118`) and UI `nodeKey` with percent-encoded segments (`treeModel.ts:53-80`) agree, folder segment included. Prefix invalidation compares whole segments and is tested on both sides.
- **The host-key flow is sound.** `trust` re-derives the fingerprint from the key and refuses a mismatch (`known_hosts.rs:100-114`). A changed key is blocked with no trust button. Writes are atomic.
- **The desktop shell is locked down.** Strict CSP, a `devCsp`, no opener permission for the UI, and Rust-only `open_log_folder`.
- **Driver test matrix is strong.** All 6 servers are tested for connect, TLS modes and snapshot introspection. Weird and Vietnamese identifiers are covered. VerifyCa uses a documented, sound workaround (`driver-mysql/src/tls.rs:89-130`).
- **UI state is disciplined.** Per-node request tokens, invalidation on close, virtualized rows and a pure, tested `computeRows`.

---

### Issues

#### Critical

None found.

#### Important

**I1. The `AppError` message contract is broken between backend and UI, and UI tests mock a shape the backend never sends.**

- **Where:**
  - Backend: `crates/core/src/error.rs:132-136` maps `DbError::Config{message}` to `message: "Invalid connection settings."`, with the real text in `detail`. `Internal` does the same (`:210-213`).
  - UI: `ImportDialog.tsx:45-49` (passphrase error), `ConnectionDialog.tsx:279-288` (field errors), `ExportDialog.tsx:128` and `DialogHost.tsx:52` all show `error.message`.
  - Tests mock the specific text as `message`: `ImportDialog.test.tsx:36` (`'Passphrase is incorrect.'`) and `ConnectionDialog.test.tsx:48`.
- **Effect:** with the real backend, a wrong `.dntconn` passphrase shows "Invalid connection settings." under the passphrase field. Spec §5.3 requires a "passphrase incorrect" message. The same generic text appears under Port, Host, SSH key, CA and similar fields. An unreadable import file shows "An internal error occurred.". The green UI tests hide this.
- **Fix:** for `Config` (and the user-facing `Internal`s), put the specific message in `message` (the drivers already write these texts for users), or show `detail` for `kind === 'config'`. Add one contract test that serializes real `DbError -> AppError` values and feeds them to the UI fixtures (or generate the fixtures from Rust).

**I2. The failing connect stage is not reported for database-side errors, so spec §7 and §10 ("display the correct step") are not met. The UI test again mocks a field the backend never sets.**

- **Where:**
  - `DbError::Connect/Auth/Tls/Server/Unsupported` carry no stage, and `From<DbError>` sets `stage` only for `Ssh` and `Timeout` (`error.rs:141-178`).
  - `SessionManager::test` returns its timings only on success (`session_manager.rs:342-373`). `open` keeps them in `last_timings`, but no command exposes them.
  - `ConnectionDialog.tsx:670` shows `test.error.stage` only. `ConnectionDialog.test.tsx:84-85` mocks `kind:'connect', stage:'connect_db'`, which never happens.
  - SSH stages arrive as `connect` / `auth` / `channel`. `STAGE_LABELS` (`ConnectionDialog.tsx:67-75`) has no entries for them, so an SSH connect failure is labelled as a raw "connect:", which is easily read as the DB connect step.
- **Effect:** a wrong DB password, a TLS failure or a refused DB port all show without a step marker.
- **Fix:** in `connect_pipeline`, attach the stage to the error, for example by returning `(ConnectStage, DbError)` and setting `AppError.stage` in the command layer, or with a `DbError::AtStage` wrapper. Return the partial timings on failure. Map the SSH stage names to labels (e.g. "SSH connect", "SSH auth").

**I3. An Overwrite import of a connection that is open keeps the old runtime, so the user browses the old server under the new name and host.**

- **Where:** `commands/transfer.rs:105-124`; `crates/store/src/transfer.rs:200-205`; `ImportDialog.tsx:129-132`.
- **Effect:** `update_connection` closes the runtime first (`connections.rs:102-116`), but `apply_import` with `Overwrite` does not. The UI also keeps `info`, so the tree keeps listing the old server's databases under the overwritten config, and object tabs keep their data. This is misleading in P1 and dangerous once P2 can write.
- **Fix:** have `ImportResult` return the overwritten ids. `apply_import_impl` calls `sessions.close(id)` for each, and the UI runs `closeConnection` / `clearInfo` for them before `reloadStored`.

**I4. The SchemaCache background load runs a full `table_detail` for every table of the default database on every open.**

- **Where:** `session_manager.rs:950-991`. Each table costs 6-8 `information_schema` queries, including the statistics query `TABLE` (`driver-mysql/src/queries.rs:34-36`) that ruling T11 deliberately kept out of listings.
- **Effect:** with 5,000 tables that is about 35k queries against what may be a production server, every time the connection is opened. It also competes with the user's tree calls for the single metadata link (`with_session` holds the tokio mutex for each call). Spec §6.3 asks only for table/view names and column names.
- **Fix:** add a bulk column query per scope: MySQL `information_schema.COLUMNS WHERE TABLE_SCHEMA=?`, PG `pg_attribute` joined to `pg_class` for the schemas in `search_path`. This can be a new `Introspector::list_columns(scope)` or one `execute`. Then the load is 1-2 queries.

**I5. `list_tree` fails as a whole if any one keychain read fails, and it reads every secret value just to compute presence flags.**

- **Where:** `crates/store/src/repo.rs:96-123` (deferred item T5).
- **Effect:** a single unreadable credential (bad encoding, Credential Manager hiccup) leaves the app with an empty tree and a banner. Every tree reload, after every move, rename or import, pulls up to 3 plaintext secrets per connection into memory.
- **Fix:** treat a per-entry error as absent and log it with the connection id only. Optionally use a presence-only lookup.

**I6. Spec §7's UI error-display rules are not implemented.**

- **Where:**
  - `open_log_folder` and `set_log_level` exist (`commands/settings.rs`) but nothing in `ui/src` calls them.
  - There is no "Internal → toast with Open log folder", no "ConnectionLost → bar with Reconnect", and no Settings switch for the debug log level (which is also not persisted).
  - `Cancelled` is filtered only through the stale-token checks.
- **Effect:** for a user, internal failures have no path to the log, and a lost connection has no Reconnect button.
- **Fix:** add a small global toast or bar driven by `kind`. Add a Settings entry for the log level and persist it in `app_settings`.

**I7. Object tabs outlive their connection, and the "not open" error cannot be recovered.**

- **Where:** `SessionManager::runtime` returns `DbError::Internal{"connection … is not open"}` (`session_manager.rs:572-576`). `closeConnection` (`tree.ts:372-399`), the edit-save path (`DialogHost.tsx:140-151`) and delete (`DialogHost.tsx:204-213`) never touch `useTabs`. `objectData.load` and Retry just call the backend again.
- **Effect:** after Close, Edit or Delete, an open tab's Refresh, Retry or an unvisited sub-tab shows "An internal error occurred.", and Retry can never succeed.
- **Fix:** return a dedicated kind (e.g. `ConnectionLost` or a new `not_open`) and have the tab offer "Reconnect" (`openConnection`, then reload). Close the tabs on delete.

**I8. SSH channel-open failures are silent (deferred T14).**

- **Where:** `crates/tunnel/src/lib.rs:342-354`.
- **Effect:** when the DB host or port is wrong behind a bastion (a very common mistake), the driver sees EOF and the user gets "Could not connect to the database server." with no hint and no SSH step. This is a §10 "correct step + hint" failure on a common path.
- **Fix:** record the last channel error in the `Tunnel`. When `ConnectDb` fails through a tunnel and a channel error was recorded, return `Ssh{stage: Channel, message}` with a hint such as "the SSH server could not reach <db_host>:<port>".

**I9. Several §10 criteria are not demonstrated by tests** (details in the §10 coverage section below).

- No test scans `app.db` or the log file for secrets.
- The `.ncx` fixture is hand-written, not a sanitized real Navicat export, which spec §9 requires.
- SSH tunnelling is tested only for MySQL 8 and PG 17.
- **Fix:** add a test that creates connections with known secrets, opens and fails a connection at Debug level, then greps `app.db` bytes and `logs/*` for each secret. Commit a real sanitized `.ncx` from Navicat 15/16 and assert its preview. Parameterize `select_one_through` over `Target::all()`.

**I10. Two spec features have backend support but no UI.**

- **Read-only flag:** there is no control for `read_only` (`ConnectionDialog.tsx:220` copies `initial?.read_only ?? false`), so it can only be set through import. Spec §1.2 and §4.5 list it as P1 scope. It has no visible effect until P2, but the setting must exist and round-trip.
- **Views:** double-clicking a View shows only the DDL sub-tab (`ObjectTab.tsx:26-28`). Spec §8 says table/view tabs have Info, DDL and Structure, and both introspectors support `table_detail` for views (`driver-mysql/src/introspect.rs:318`, pg `relkinds`).
- **Fix:** add a "Read-only" checkbox on the General tab, and include `View` in `subTabsOf`.

#### Minor

- **M1.** UTF-8 BOMs are still present in `.gitignore`, `crates/core/Cargo.toml`, `crates/core/src/meta.rs` and `rust-toolchain.toml`, which breaks the global no-BOM rule (deferred T1). Strip them.
- **M2.** The environment-tag colour is not shown on tabs, although spec §4.5 asks for tree, tab and status bar (`TabBar.tsx:24-40`). The status bar follows the tree selection, not the active tab (`StatusBar.tsx:7-10`).
- **M3.** A Saved-mode connection whose SSH key is encrypted and whose passphrase is not stored fails with an `ssh/auth` error in the tree instead of prompting (`session_manager.rs:131-140`, `tree.ts:274-278`). Map "key is encrypted: a passphrase is required" to `password_required` with the need `ssh_key_passphrase`.
- **M4.** Nothing checks committed `ui/src/bindings.ts` against the Rust types (`tests/commands.rs:150-167` only checks substrings in a temp export). Add a test that compares the exported text with the committed file.
- **M5.** The passphrase length rule differs between layers: the UI counts UTF-16 units (`ExportDialog.tsx:101-103`), the backend counts `chars()` (`dntconn.rs:89`). Non-BMP passphrases pass the UI and then fail with the generic message from I1.
- **M6.** `update_connection_impl` closes the runtime before `repo.update` (`connections.rs:110-113`). An `open` that lands in between installs a runtime with the old config. Close again, or bump the generation, after the update.
- **M7.** `Value::Bytes.preview` serializes as a number array, not hex as in spec §3.1 (`value.rs:46-51`). Not used in P1; align before P3.
- **M8.** Error boundaries cover only Sidebar and Workspace. A crash in `DialogHost`, `Toolbar` or `StatusBar` blanks the whole app (`App.tsx:36-50`).
- **M9.** The host-key mismatch text says to remove the old key "from known_hosts manually" but never names the app-owned file (`HostKeyDialog.tsx:43`, `ConnectionDialog.tsx:663-664`). Show `%APPDATA%\DntDatabase\known_hosts`, or offer "Forget key" behind a confirmation.
- **M10.** An unreadable CA, certificate or key file in MySQL returns `DbError::Tls` instead of `Config{field}` (`driver-mysql/src/tls.rs:62-66`), so it is not routed to the field. Import file read errors are `Internal` (`commands/transfer.rs:47-55`).
- **M11.** Some blocking work still runs on async worker threads, contrary to ruling T16: SQLite `repo.get` in the browse commands (`browse.rs:125,143`) and keychain reads in `resolve_secrets` / `test` (`session_manager.rs:351-364, 475-506`).
- **M12.** E2E depends on a manual `--features tauri/custom-protocol` rebuild that other cargo commands silently overwrite (`e2e/wdio.conf.ts:9`, README:79-84). Add a `custom-protocol` feature and build into a separate `--target-dir` from the E2E script.

---

### Deferred-minor triage

**Must fix before merge**

- **T5:** `tree()` fails wholly on one keychain read. This is I5.
- **T14:** silent channel-open failures. This is I8.
- **T16:** test gap for a secret-free log file. It is a §10 criterion; part of I9.
- **T1:** BOMs in 4 files. Trivial, and a global rule. This is M1.

**OK to defer** (one line per task; items marked "before P2" are cheap and safety-relevant once queries run)

- **T1:** value test gaps; early `thiserror`/`uuid` dependencies. OK to defer.
- **T2:** drift test for `as_str`; 28000 vs 1045 ordering (MySQL errors always carry a code); test gaps; `Server` message equal to detail. OK to defer.
- **T3:**
  - Parenthesised `WITH` → Other; multibyte split tests; the backtick/INTO/`*/` edge cases: OK to defer.
  - PG `'C:\'` inside `WITH` misjudged Write→Read: OK for P1, **must fix before P2** (Production confirmation).
- **T4:** real-world version strings in the test; MariaDB `system_versioned` ≥ 10.3.4 gate; `ssh.port` 0 accepted by `validate()`. OK to defer.
- **T5:** non-atomic update/delete vs keychain; `group_id` → Internal; failing-store tests. OK to defer.
- **T6:** Vec reallocation residue; generous KDF caps; swallowed keychain cleanup; an invalid group aborting the import; tests. OK to defer.
- **T7:**
  - Non-zeroized decrypted plaintext: OK to defer (memory hygiene only), but cheap.
  - SSH secrets decrypted when SSH is off (orphan credentials): OK to defer.
  - Dead loop; quick-xml text in errors; tests: OK to defer.
- **T8:** PG fixture type coverage; `smoke.rs` hardcoded port; tautological asserts. OK to defer.
- **T9:** `value_text` quoting; pinning the rustls downcast; URI-only SAN message. OK to defer.
- **T10:**
  - DATETIME(1) formatting; `USE` after a comment: OK to defer.
  - `fetch_max` for cancel generation; deadlock/cancel ordering; Timeout reported after a commit: OK for P1, **fix before P2** (execute path).
- **T11:** backtick SHOW test; 5.7 warm cache; silent column drop on decode failure. OK to defer (see I4 for the related performance issue).
- **T12:** stuck-timeout replacement path; grace-period Timeout; binary/hex for unmapped types; BC timestamps; `search_path` after a comment. OK for P1 (no query UI), **before P2**.
- **T13:** missing per-column COLLATE, identity options, COMMENT ON for constraints and indexes; partitioned parent size; extra round trip; `qualify_reference` edge case. OK to defer, but note that "Copy DDL" is lossy for collations; track for P4.
- **T14:** blocking key decrypt; IO errors labelled Auth; parsing tests; concurrent trust lost update; RUSTSEC-2023-0071 (rsa timing, client-side signing only, low risk). OK to defer; track the advisory.
- **T15:** swallowed SchemaCache errors; no SchemaCache reload on refresh; blocking keychain reads; test gaps; non-atomic close critical section; timings not generation-gated; orphan session after a forced shutdown; superseded open running until timeout; 5 s fallback untested. OK to defer (I4 should be fixed regardless).
- **T16:** placeholder `ui/dist` can ship; default panic hook prints to stderr (the release build has no console); `devCsp` lacks 127.0.0.1. OK to defer.
- **T17:** scroll fight; context-menu a11y; aria-setsize; DRY; hardcoded UTF-8; `noUnusedLocals`; two lockfiles; stuck-loading after a close failure; open/close IPC race (the backend is consistent, the next expand reuses the runtime); `[object Object]`. OK to defer.
- **T18:** MariaDB chip lost on edit; ports silently defaulted; focus trap; test mutating the store; HostKey re-enable; stale field error; cleared password still tests with the stored one. OK to defer.
- **T19:** swallowed refresh failure; aria-label; module-level subscribe; literal `'NULL'` default shown as —. OK to defer.
- **T20:** ActionMenu a11y; notice timer; indeterminate checkbox; tests; file changing between preview and apply; reload throw re-enabling Import; `chip-ma` class. OK to defer.
- **T21:** Ctrl+C cleanup; credential-diff race; action pins; double CI runs; unanchored `tools/`. OK to defer.
- **Cross-task:** plaintext password kept in `mysql_async::Opts` / `tokio_postgres::Config` for reconnect. **OK to defer** as a documented library limitation: the libraries take `String`, neither prints it in `Debug` (no derive on `MySqlSession`, and tokio-postgres redacts), and the lifetime equals the session's. Document it in the constraints and revisit if the libraries gain secret-aware APIs.

---

### Rulings I disagree with

None is outright wrong. Three have caveats worth recording:

- **T11 (list_objects never reads stats)** is right on its own. However, the SchemaCache loader (T15) calls `table_detail` per table and so brings back the very statistics cost the ruling avoided, at a much larger scale. See I4.
- **T18 (test falls back to stored secrets for an existing id)** is acceptable and matches Navicat. Note the side effect: editing a Production connection's host and clicking Test sends the stored Production password to the newly typed host. Consider applying the fallback only when host, port and user are unchanged, or saying "using saved password" in the result.
- **T3 (dialect-less `classify`)** is fine for P1, where `classify` gates nothing. It must be revisited before P2, because a PG Write misclassified as Read would skip the Production confirmation.

---

### Spec §10 coverage

| Criterion | Status | How it is (or is not) demonstrated |
|---|---|---|
| CRUD, duplicate, drag and drop of connections and groups; data survives an app restart | Covered | `store/tests/repo.rs` (incl. `reopen_persists`), `ui/src/store/tree.test.ts` (move_node), `DialogHost.test.tsx` |
| Connect to all 6 servers directly | Covered | `driver-*/tests/connect.rs` over `mysql_family()` / `pg()` |
| Connect to all 6 servers via SSH | **Partial** | Only MySQL 8 and PG 17 (`tunnel/tests/tunnel.rs`, `session_manager_db.rs:103`) |
| TLS `Require` works | Covered | Both `connect.rs` files |
| Variants detected correctly; tree shows the right objects per variant; Info/DDL/Structure match snapshots | Covered for backend | Unit + integration tests and insta snapshots. The UI renders only fake data; views lack Info/Structure (I10). |
| Passwords only in Credential Manager (app.db and logs contain no secret) | **Not demonstrated** | Only command output is checked (`tests/commands.rs:30`); no scan of app.db or log files (I9) |
| `.dntconn` export → import round trip, with and without passwords | Covered | `store/tests/dntconn.rs`, `tests/commands.rs:96` |
| Import a `.ncx` from real Navicat | **Not demonstrated** | The fixture is synthetic (I9) |
| Every connection error shows the correct step with a hint | **Not met** | Hints are unit-tested, but the step is missing for DB-side errors (I2) and SSH channel failures (I8) |
| App does not crash on errors | Covered | `guard`/`blocking` panic tests; UI error boundaries (partial, M8) |
| Test tiers 1–3 green | Covered | Per task reports and CI |
| E2E smoke green on Windows | Covered (manual) | Per task report; not in CI, which the spec does not require; fragile build step (M12) |

---

### Declined to judge

- **Tunnel listener on `127.0.0.1:0` accepting any local process:** standard `ssh -L` behaviour, Navicat does the same, and the database still authenticates. Outside the spec.
- **A compromised webview using the `path` arguments of export/import to write or read arbitrary files:** this needs XSS under a strict CSP; the frontend is part of the trusted app. Outside the threat model in the spec.
- **Driver-level reconnect of query sessions after a tunnel reopen uses the old local port:** P1 opens no query sessions; this is P2's concern.
- **Performance of the MySQL 5.7 cold `information_schema` path:** ruled on in T11; I did not run it.
- **Correctness of the Navicat 12 AES key/IV and the known-pair vectors:** covered by the T7 task review; not re-derived.
- **Visual fidelity to the canvas (spacing, tokens):** not checkable without running the app; task reviews covered it.
- **tauri-specta, specta and russh pinned at release-candidate or exact versions:** a deliberate dependency choice, not a defect.

---

### Recommendations

1. Fix I1 and I2 together: they share one root cause, the `AppError` contract. Make `message` user-facing for config errors and carry the stage. Then add a contract test that turns real `DbError` values into AppErrors and runs them through the UI's message and stage rendering, and rebuild the UI fixtures from it.
2. I3, I5 and I7 are small, local fixes (close runtimes on overwrite; per-entry keychain errors; a not-open error kind plus close/reconnect handling for tabs).
3. I4: add one bulk column query per variant before anyone points the app at a large production schema.
4. I8, I9 and I10 close the §10 gaps. For I9, a real sanitized `.ncx` needs a human with Navicat installed.
5. Before P2, schedule: T3 (PG classification), T10 and T12 (timeout/commit reporting), the I6 ConnectionLost bar with Reconnect, and an environment-tag colour on tabs (M2).

### Assessment

**Ready to merge?** With fixes

**Reasoning:** The architecture, secret handling and concurrency are solid, and nothing is critical. But the AppError contract between backend and UI is broken in ways the mocked UI tests hide (I1, I2), an Overwrite import leaves stale sessions (I3), and several §10 completion criteria (error step, secret scan, real `.ncx`) are not yet met or not demonstrated.
