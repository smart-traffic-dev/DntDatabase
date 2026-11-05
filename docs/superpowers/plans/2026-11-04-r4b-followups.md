# R4b — hand-off for the MongoDB and Redis driver specs

R4b (`docs/superpowers/plans/2026-11-04-r4b-command-family-ui.md`) is done on branch `r4b-command-ui`. The UI handles
a Command connection end to end against the R4a backend: dialog, tree, console, Table/JSON results, data view and
edits, and the key browser. Everything is proven against `FakeCommandDriver` and the e2e-web fake. No real driver is
registered yet, so in a release build only the connection dialog and the "no driver" message are live.

Verification at the branch tip: fmt, clippy, `cargo test --workspace`, UI lint (with the new kind-literal rule), the
UI suite (1871 tests), e2e-web (51 specs) and the e2e-web typecheck all pass. Running the Rust suite in a worktree
needs the gitignored `testdata/ssh` and `testdata/certs` copied in.

## Decisions to carry forward

- **Engine module.** Every per-kind decision lives in `ui/src/engine/` as an exhaustive switch. ESLint forbids
  comparing a kind with a literal anywhere else. A new kind breaks the TypeScript build until each switch handles it.
  - `isKind` takes `SqlEngine`, not `DbKind`.
  - SQL-only code gets its engine once from `sqlEngine(kind)` and shows nothing when that is `null`.
- **Missing driver.** `SessionManager::driver` returns `DbError::Config` with "This build has no <Engine> driver
  yet." (no longer `Internal` "no driver registered for …"). The engine picker lists `availableEngines()`. A saved
  connection of a driverless kind can still be edited and saved, but not tested or connected.
- **Defaults.** New Mongo/Redis connections default to SSL `Disable`. Switching engine resets `options.driver`.
- **`DatabaseInfo.key_count`.** The tree labels `numbered_db` databases `db0 · 1,234 keys`. The Redis driver must
  fill it from `INFO keyspace`; SQL drivers return `None`.
- **`CellInput::Unset`.** Removes a field. It is Command-only; SQL edit builders refuse it with
  "Removing a field is not available for this engine".
- **`DbError::KeyNotFound`.** It has AppError kind `key_not_found` and text "Key no longer exists".
  `Keyspace::key_value` must return it for a missing key.
- **Statement ranges.** A Command tab asks the backend (`statementRanges`, debounced 150 ms). The `dialect` argument is
  ignored when the connection has a command language.
- **JSON view.** It rebuilds documents from result rows and skips every `Null` cell, because run events carry no
  padding mask. So a real `field: null` is not shown (plan decision 6). A driver that wants it shown must add a
  per-cell presence mask to result events; `rowToDocument` already accepts one.
- **Key browser.**
  - Key tabs are identified by `keys:<connectionId>:<database>` and persisted in localStorage (`dnt.keys.tabs`), not in
    the SQLite tab store.
  - A key is identified by `KeyName.raw`.
  - No value is ever edited or saved from a truncated preview, nor from bytes that are not valid UTF-8 (those can only
    be deleted or removed).
- **Production key edits.** They confirm without a typed name, because `EditPreview` has no confirm name. The backend
  verdict still forces Confirm.
- **Hidden for the Command family:** Format SQL, Explain, manual transactions, Copy as INSERT, Export on query results
  and on data views, Select top 1000, and Open in query. Collection DDL is shown as JSON.
- **`ADMIN` label.** The run-confirm label for `admin` commands is `ADMIN`, matching the other uppercase labels.

## Driver specs must state

- **New key and Rename must not destroy an existing key.**
  - Today "New key…" sends `set_string` (`SET`), which replaces a key of any type, and "Rename…" sends `RENAME`, which
    drops the destination.
  - Decide in the backend contract: `SET … NX` / `RENAMENX` variants, or a Confirm verdict from `build_key_edit` when
    the target exists.
  - The UI must not check this on its own.
- **Hash and set paging.** `ValueRange` pages by offset, but `KeyValue::Hash` / `Set` return a `next` cursor that the
  UI cannot send back. A real `HSCAN` / `SSCAN` cannot honour an offset. Add a cursor to `ValueRange`, or page these
  types by cursor.
- **List "Remove item N".** It sends `LREM key 1 value`, which removes the first match, not item N, when the list has
  duplicates. Add an index-based removal or relabel the action "Remove first match".
- **Vanished keys.** A `KeyNotFound` marks the pooled data session unclean (one reconnect per vanished key). The Redis
  driver should treat it as a clean outcome.
- **Fidelity of the e2e fake.** Its Mongo rows are `{_id, document}`. A real driver flattens top-level fields
  (spec §7.3), and the e2e specs should follow once a driver exists.

## Small follow-ups (UI)

- **Keyspace tree nodes.**
  - Add an "Open keys" menu item; today the browser opens only by double-click or Enter.
  - "Copy name" copies the label (`db0 · 5 keys`), not the database name.
  - Refresh does not update the key count after the key browser adds or deletes keys.
- **Export shortcut.** Ctrl+Alt+E on a Command data view still builds an export request (`dataExportRequest`). The
  backend refuses "All rows"; hide it like the menu item.
- **Connection dialog.** The `ssh` field error clears only when SSH is toggled, not when SRV is turned off.
- **Key browser polish.**
  - The pattern, type and add-row inputs are unstyled native controls.
  - The list has `role="listbox"` without arrow-key movement.
  - Switching an invalid Hex draft to Text drops the draft.
- **JSON view.**
  - It has no syntax colouring.
  - Base64 of a truncated `Bytes` value is not marked as truncated.
  - A field named `__proto__` is lost.
  - "Copied" never resets.
- **Hygiene.**
  - `rangeSeq` and `jsonViews` maps are never pruned.
  - `keysTabs` imports `sameKey` from the tab UI folder.
  - `RunConfirmDialog` compares `plan.family` with a literal.
- **Type cleanup.** `columnModel.ts` re-declares `SqlEngine`, and a few files spell `'mysql' | 'postgres'` instead of
  using it.
- **Commit message.** The subject of commit `594c349` starts with a UTF-8 BOM. Fix it only if the branch is squashed.
- **Flaky e2e.** `ai-suggest` "Esc dismisses the suggestion" failed once in a full e2e run and passed alone.
