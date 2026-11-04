# F2 — AI in the app backend, Settings → AI and consent Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The app can store AI settings and an app-level API key, test the endpoint, record per-connection consent,
and answer `ai_complete` for a query tab. The Monaco ghost text that calls it is F3.

**Architecture:** `crates/store` gains the `ai_opt_in` table and app-level secrets. `src-tauri` gains `ai.rs`
(`AiSettings`, `AiManager`: client cache, pause state, in-flight requests) and `commands/ai.rs`. The UI gains a
Settings → AI tab and the consent dialog.

**Spec:** `docs/superpowers/specs/2026-11-04-ai-editor-completion-design.md` §3, §6, §7, §8, F2 in §11. Read
`docs/superpowers/plans/2026-11-04-f1-followups.md` first.

## Global Constraints

- **Existing behaviour unchanged;** every existing test passes, except the two migration-10 tests, which now
  check the latest version and ignore the new table.
- **The API key crosses IPC only in `ai_key_set`.** No command returns it; `AiSettings` says `has_key` only.
- **Prompt, suffix and suggestion text are never logged.**
- **AI errors are never `Internal`** (the UI reports `Internal` as a bug): `AiError::Config` → `DbError::Config
  { field: Some("ai") }`, `Unauthorized` → `DbError::Auth`, everything else → `DbError::Connect { message }`.
- **Opt-in is per connection and per origin:** a row counts only while its `origin` equals the current endpoint's
  origin. Rows are kept when the base URL changes, so switching back restores them. Deleting a connection deletes
  its rows (foreign key cascade). `.dntconn` / `.ncx` never carry them (they live in their own table).
- **Identifiers, error text and UI copy are in English.**
- **Bindings:** `cargo run -p dntdatabase --example export_bindings`; `bindings_are_current` must pass.
- **Verification per task:** `cargo check --workspace --all-targets`, the task's tests, `cargo fmt --all`,
  `cargo clippy --workspace --all-targets -- -D warnings`; UI tasks also `pnpm --dir ui lint` and `pnpm --dir ui test`.
- **Decisions this plan makes:**
  1. Opt-in is turned on from Settings → AI ("Turn on for a connection…"). The connection dialog and the query-tab
     status bar entry points come with F3, next to the editor.
  2. The app-level key is stored as `DntDatabase/app/ai_api_key` in the same keychain service.
  3. While paused (spec §7) `ai_complete` answers `Paused { message }` instead of calling the endpoint.
  4. `ai_complete` uses `Dialect::for_kind` and the connection's schema cache (loaded once, as completion does).

## Tasks

### Task 1: `ai_opt_in` table and repository API (`crates/store`)

- Migration 11: `CREATE TABLE ai_opt_in (connection_id TEXT NOT NULL REFERENCES connections(id) ON DELETE CASCADE,
  scope TEXT NOT NULL CHECK (scope IN ('editor')), origin TEXT NOT NULL, granted_at TEXT NOT NULL,
  PRIMARY KEY (connection_id, scope))`.
- `AiScope { Editor }`, `AiOptIn { connection_id, scope, origin, granted_at }`;
  `ConnectionRepo::{ai_opt_in_grant(id, scope, origin), ai_opt_in_revoke(id, scope), ai_opt_ins(scope)}`.
- Tests: grant/revoke/list, grant replaces origin, deleting the connection removes the row, unknown connection is
  `NotFound`, a `.dntconn` export of an opted-in connection has no AI field. Migration-10 tests updated.
- Commit `feat(store): ai_opt_in table`.

### Task 2: App-level secrets (`crates/core`, `crates/store`)

- `AppSecret { AiApiKey }`, `app_secret_key(AppSecret) -> "DntDatabase/app/ai_api_key"`;
  `SecretStore::{get_app, has_app, set_app, delete_app}` with defaults returning `Unsupported` is **not** used:
  every store implements them (memory, keyring, any test store).
- Tests: memory round-trip; keyring round-trip `#[ignore]` like the existing one; connection secrets untouched.
- Commit `feat(core): app-level secrets`.

### Task 3: AI settings, `AiManager` and the settings commands (`src-tauri`)

- `ai.rs`: `AiTrigger { Automatic, Manual, Off }`; `AiSettings { base_url, model, trigger, max_tokens, timeout_ms,
  max_lines, has_key, origin }`, `AiSettingsInput` (same without `has_key` / `origin`). Keys `ai.base_url`,
  `ai.model`, `ai.trigger`, `ai.max_tokens` (16–512, default 128), `ai.timeout_ms` (1000–30000, default 5000),
  `ai.max_lines` (1–20, default 8). Saving validates the base URL with `Endpoint::parse` (empty = not set).
- `AiManager` in `AppState`: builds an `Arc<dyn FimClient>` from settings + key on demand and caches it until
  settings or key change (a factory is injectable for tests); pause state; in-flight requests by id.
- Commands: `ai_settings_get`, `ai_settings_set`, `ai_key_set`, `ai_key_clear`, `ai_models`, `ai_test`,
  `ai_opt_in_list`, `ai_opt_in_set(conn_id, enabled)`. `ai_test` returns `{ verdict, elapsed_ms, sample }`.
  `ai_opt_in_set(true)` needs a base URL; it records the current origin.
- Tests (`src-tauri/tests/ai.rs`, fake client): defaults, validation messages, key never in `AiSettings`, cache
  rebuilt after a change, opt-in follows the origin, test verdicts, error mapping is never `Internal`.
- Commit `feat(app): AI settings, key and consent commands`.

### Task 4: `ai_complete` and `ai_cancel` (`src-tauri`)

- `ai_complete({ request_id, conn_id, database, schema, text, cursor }) -> AiCompletion` where
  `AiCompletion = Suggestion { text } | Empty | Paused { message }`.
  - Not configured, connection not opted in for the current origin, a Command connection, or `Skip::InSecret`
    → `Empty`.
  - Builds the prompt from the schema cache, calls the client in a task registered under `request_id`,
    cleans the reply with the settings' `max_lines`.
  - Pauses: `Unauthorized`, `FimUnsupported`, `BadRequest` until settings or key change; `RateLimited` for
    `retry_after` (default 30 s). Other errors return the error and do not pause.
- `ai_cancel(request_id)` aborts the task; a cancelled `ai_complete` returns `Empty`.
- Tests: suggestion end to end with the fake client and the fake driver's schema; not opted in → no call;
  pause after 401 and resume after `ai_key_set`; 429 pause expires; cancel; secret cursor → no call.
- Commit `feat(app): ai_complete`.

### Task 5: Settings → AI and the consent dialog (`ui`)

- Bindings regenerated. `store/ai.ts` (zustand) wraps the commands.
- Settings tab **AI**: Base URL, API key (password field, Save / Clear, "A key is saved."), Model (text + Load
  models), Editor suggestions (Automatic / Manual (Alt+\) / Off), Max tokens, Timeout, Max lines, Test (result
  line), "Connections with AI" (name, Turn off) and "Turn on for a connection…" (select + button).
- `AiConsentDialog`: names the endpoint host and what is sent (editor text with passwords hidden, table and
  column names; never row data). `Turn on AI` is disabled until "I am allowed to share this database's schema and
  queries with this endpoint" is ticked; initial focus and Enter are Cancel.
- Tests (vitest): fields load and save, validation errors shown, key never echoed, Test result, consent gating and
  Enter = Cancel, Turn off.
- Commit `feat(ui): Settings → AI and AI consent`.

### Task 6: e2e-web handlers, verification, hand-off

- `e2e-web/src/backend.ts`: handlers for the new commands (AI not configured by default).
- Full: `cargo test --workspace`, fmt, clippy, `pnpm --dir ui lint`, `pnpm --dir ui test`, bindings check.
- `docs/superpowers/plans/2026-11-04-f2-followups.md` for F3.
- Commit `docs(ai): F2 hand-off`.
