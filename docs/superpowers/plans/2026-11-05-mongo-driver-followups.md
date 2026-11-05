# MongoDB driver — hand-off

The plan `docs/superpowers/plans/2026-11-05-mongo-driver.md` is done on branch `mongo-driver`. It implements spec rev 2,
`docs/superpowers/specs/2026-11-05-mongo-driver-design.md`.

- `MongoDriver` is registered in `default_drivers()`.
- Mongo connections work end to end: dialog, tree, console, Table/JSON results, data view, and document edits. Edits on a
  replica set are atomic.
- Every task passed review. The final whole-branch review found:
  - a Critical issue: a commit failing with a write-concern error was reported as rolled back;
  - two Important issues: SRV authSource, and the SQL DB suites not being run.

  The code fixes landed and were re-reviewed. The SQL DB suite run is still open.

Verified at the branch tip:

| Check | Result |
|---|---|
| `cargo fmt` / clippy `-D warnings` | clean |
| `cargo test --workspace` | 2183 passed |
| Mongo DB-backed tests (MongoDB 6.0 standalone + 8.0 replica set with TLS) | 147 passed |
| App end-to-end tests (`src-tauri/tests/mongo_driver.rs`) | 6 passed |
| UI lint / tests | 1892 passed |
| e2e-web | 51 passed |
| `Cargo.lock` | no `aws-lc-rs`, `aws-lc-sys`, `openssl-sys` or `native-tls` |

**Not yet run:** the SQL DB-backed suites (`DNT_TEST_DB=1`) against the full `dnt-test` stack. Run them before merging.
`bson` turns on `serde_json/preserve_order` for the whole app, so JSON maps keep their source key order instead of
sorted order. Unit tests and snapshots are unchanged.

## Decisions carried forward

**Core contract changes**

- `CommandSession::execute_atomic` and `AtomicFailure` were added.
  - The backend calls `execute_atomic` only when the plan is atomic and the engine reported transactions at preview time
    (`CommandEdit.transaction`).
  - An unknown commit outcome makes every row `outcome_unknown`.
  - An `Unsupported` at index 0 means nothing was sent.
- `ResolvedConfig.via_tunnel` is an explicit flag, set in `session_manager`. It is not inferred from the host.
- The Command fingerprint hashes only the key columns' types and the index list.
- Command text in previews and History is cut at 4 KiB, and a cut row is marked redacted.
- Raw filters for the Command family are no longer pre-checked by the app; the driver validates them.
- `list_objects` gained `show_system`. Mongo hides `admin`, `local`, `config` and `system.*`.
- Mongo Verify full no longer requires a CA file and trusts the bundled roots. SQL and Redis validation is unchanged.
- Mongo URIs accept Atlas driver options. A missing `authSource` takes the path database, except for `mongodb+srv`, where
  the TXT record decides.

**Console language** (`crates/driver-mongo/src/shell`)

- Parsing:
  - The grammar is closed; JavaScript is refused.
  - Splitting uses a JS-faithful strict lexer, checked against acorn in the fuzz harness.
  - A bad region spanning several lines is one span. Unclosed templates, comments and continued strings run to the end
    of the text.
  - A bare `use` name takes only name characters; quote anything else. This is stricter than spec §4.2/§144, which only
    lists `/\. "$*<>:|?` as invalid.
- Classification:
  - `runCommand` / `adminCommand` refuse `$` keys and session fields (`lsid`, `txnNumber`, `startTransaction`,
    `autocommit`).
  - Lowercase aliases stay `Unknown`, which fails closed.
  - Modifiers after `aggregate` become pipeline stages.
- Redaction has two tiers:
  - Commands that parse are redacted precisely.
  - Any region that does not parse and contains a marker is replaced entirely by `***`. Markers: the substrings
    `pwd` / `password` / `secret`, the whole word `auth`, any backslash, and `mongodb`.
  - A URI has a password when a `:` appears before the first `@`.
- Extended JSON wrappers beyond spec §4.3 are accepted so that `canonical_ejson` output round-trips: `$symbol`, `$code`,
  `$scope`, `$dbPointer` and `$undefined`.
- The regex flag `g` is an error.
- On read-only connections, only `queryPlanner` explain is allowed.

**Execution**

- The read-only allowlist (`exec::allowed_read_only`) never reads `class`.
- A `runCommand` body's first key is never changed: a leading `maxTimeMS` is left in place.
- `consideredValue` is stripped from validation errors, and server error messages are redacted.
- Cancel:
  - `killOp` is matched by comment, and the canceller kills only when the command's own stop is raised.
  - After the grace period the `Client` is rebuilt.
  - An outside `killOp` is reported as `Server { Interrupted }`.
- A write past `opts.timeout` gives `Timeout { Execute, outcome_unknown }`.
- Filter values are converted to the sampled field types at execute time, through one extra 100-document sample. Such
  reads carry the comment `// values are converted to the field types sampled from the collection`. `Contains` is
  unanchored; `Like` and `EndsWith` end with `\z`.
- Edits:
  - Command text uses canonical Extended JSON values. It round-trips through our console but is not pasteable into
    mongosh.
  - The `mongodb` crate's own commit/abort retries are accepted, because commit is idempotent.
  - A write-concern error on commit or on an edit means the outcome is unknown, never "rolled back".

**Test infrastructure**

- The compose services `mongo60` (:27060) and `mongo80` (:27080, `rs0`, TLS, keyFile) run with `enableTestCommands=1`.
- `testkit` has the targets `Mongo60` and `Mongo80`.
- `gen_fixtures` writes the keyfile and the combined client PEM.
- Run them under an isolated project: `docker compose -p dnt-mongo-wt …`.

## Open items

**Parked**

- A `use` inside an open bracket. `split` resets bracket depth after a bad region. With the cursor target, a later line
  `use x` inside the same brackets can still become its own runnable span (`f({\n a: 'x\nuse prod\n})`). A script with
  any blocked span runs nothing, and the tab's db change is visible.
  - Fix: carry bracket depth across pieces in `split`.

**Deferred**

- Write-concern errors in cursor command replies (aggregate `$out` / `$merge`) are not checked yet.
- The `ServerSelection` and `Authentication` error messages are not redacted.
- `execute_atomic` gaps:
  - no `maxCommitTimeMS` when there is no timeout, so Stop can't interrupt a slow commit;
  - an unconfirmed abort leaves the server transaction open until its lifetime limit;
  - no stop/timeout test inside a unit.
- Test gaps:
  - date and embedded `_id` values are not tested end to end;
  - `stop_during_the_sample_sends_no_read` is flaky under parallel load.
- Smaller items:
  - a filtered read can take about twice the timeout;
  - `int` overflow silently widens to Int64;
  - `codeWithScope` hides its scope;
  - Command cut cells show no "preview only" hint;
  - the `runCommand renameCollection` drop keeps the unsplit namespace.
- File size: `exec.rs` and `parse.rs` are large; split `exec/plan.rs` and the `split` module.
- Spec text to update:
  - §6.2: `Contains` is unanchored, `_id_` is `unique: true`, `Like` ends with `\z`;
  - §4.2: the bare `use` name rule.
- Atlas UX: a URI with a path database and no `authSource` uses the path database for non-SRV connections, per MongoDB
  semantics. A hint in the dialog would help.

**For the Redis plan** (`2026-11-05-redis-driver.md`)

- `execute_atomic` exists now.
- `ValueRange.cursor`, `KeyEdit::Create` and the `ListRemove` shape change are still pending in that plan.
- `validate_redis` still requires a CA file for Verify full.
