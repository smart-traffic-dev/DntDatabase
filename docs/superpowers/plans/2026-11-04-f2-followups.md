# F2 — hand-off for F3 (Monaco ghost text, entry points)

F2 (`docs/superpowers/plans/2026-11-04-f2-ai-backend-settings.md`) adds the `ai_opt_in` table, app-level secrets,
`src-tauri/src/ai.rs` (`AiConfig`, `AiManager`), the `ai_*` commands, Settings → AI and `AiConsentDialog`.

## What F3 builds on

- **`ai_complete({ request_id, conn_id, database, schema, text, cursor })`** returns
  `{ kind: 'suggestion', text } | { kind: 'empty' } | { kind: 'paused', message }`, or an error for a transient
  failure (timeout, network, 5xx: never `internal`, so the UI does not report a bug). `cursor` is Monaco's model
  offset as is. The backend checks everything (set up, trigger not `off`, consent for the current origin, SQL
  family, cursor not in a secret); the UI may skip a call early but must not rely on that.
- **`ai_cancel(request_id)`** aborts it; the pending `ai_complete` then resolves `empty`. A new request with the
  same id aborts the older one, so one id per editor is enough.
- **Pause:** after 401 / 403, a 4xx about `suffix`, or another 4xx, `ai_complete` answers `paused` without calling
  the endpoint until the settings or key change; after 429 for `Retry-After` (30 s by default). The status-bar
  indicator can show `message`.
- **Settings for the editor:** `ai_settings_get().config.trigger` (`automatic` | `manual` | `off`). Read it when
  the editor mounts and again after Settings closes (there is no change event).
- **Consent per connection:** `ai_opt_in_list()` gives `active` per connection; `ai_opt_in_set(conn_id, true)` must
  only be called after `AiConsentDialog` (`ui/src/dialogs/AiConsentDialog.tsx`) was confirmed.

## Choices made in F2

- Consent is turned on from Settings → AI only. The connection dialog and the query-tab status bar ("AI off · Turn
  on…") are F3, next to the editor.
- `AiManager` is a field of `AppState` (`Arc<AiManager>`); tests swap the endpoint client with
  `state.ai.set_client_factory`.
- `AiError` → `DbError`: `Config` → `Config { field: "ai" }`, `Unauthorized` → `Auth`, anything else → `Connect`.
- The key entry is `DntDatabase/app/ai_api_key` in the same keychain service as connection secrets.
- Settings → AI saves the endpoint fields with one Save button; Test needs the saved settings (disabled while
  there are unsaved changes).
- The schema header comes from `SessionManager::schema_cache_for` (loaded on first use, like completion). A closed
  connection gives no tables, not an error.

## Not verified in F2

- No real endpoint was called from the app; the backend tests use a scripted client, and F1's smoke test covers
  the HTTP client.
- `e2e-web` typecheck has two errors that predate F2 (`ParsedConnString.driver` in `src/backend.ts`, `env` in
  `specs/data-edit.spec.ts`); the AI handlers added there typecheck.

## Open items

- Command family suggestions: MongoDB done (console prompt with `//` header, the console redaction refuses any
  secret; `mongo-assist`). Redis pending: `ai_complete` still returns `empty` for it.
- `ai_complete` re-reads the settings and the consent table on every call (a few small SQLite reads); cache them
  in `AiManager` if F3 measures it as too slow.
