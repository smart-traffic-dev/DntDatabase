# F3 — AI ghost text in the query editor: what was built and what is left

F3 completes the AI editor completion spec (`docs/superpowers/specs/2026-11-04-ai-editor-completion-design.md`)
on top of F1 (`crates/ai`) and F2 (backend, Settings → AI). It was built straight from the spec and the F2
hand-off, without a separate plan file.

## What F3 adds (`ui/`)

- `store/ai.ts` (`useAi`): settings and consent as the editor needs them, plus the indicator state (in-flight
  requests, pause message, consecutive failures). Loaded once with `commands` (not `api`), so a backend without the
  AI commands never raises the internal-error dialog. Settings → AI updates it on every save.
- `query/ai/inlineProvider.ts`: a Monaco `InlineCompletionsProvider` for `mysql` and `pgsql`.
  - Skips (spec §5.1): no connection, AI not loaded / not set up, trigger `off`, `manual` without Alt+\, no
    consent for the connection, a selection, several cursors, the suggest list open, the cursor inside a word.
  - Automatic requests wait 350 ms; Monaco cancelling the token (typing on) drops it before or after the call
    and sends `ai_cancel(ai-<tab id>)`.
  - LRU cache of 50 answers by (connection, database, cursor, text hash).
  - Tab / Ctrl+→ / Esc are Monaco's own inline-suggest keys; `bindAiKeys` adds Alt+\ as an editor action.
- `components/AiStatus.tsx`: the status-bar indicator of a query tab with a connection, shown once AI is set
  up: `AI off`, `AI` (on, working, paused, failing). Its menu turns AI on (through `AiConsentDialog`) or off
  for the connection and opens Settings → AI.
- `SqlEditor`: registers the provider and Alt+\, `inlineSuggest.enabled`.

## Verified

- vitest: provider gating, debounce, cancel, cache, pause / failure accounting; indicator states, consent from the
  status bar, hidden until set up; whole UI suite.
- e2e-web in Chromium with the real Monaco (`specs/ai-suggest.spec.ts`): ghost text after a pause in typing, Tab
  inserts it, Esc dismisses it, manual mode asks only on Alt+\, the request carries the connection, text and
  UTF-16 cursor.

## Not verified

- The desktop app against a real FIM endpoint (needs the user's base URL, model and key).

## Known and left as is

- Vite's dev server logs `[Unhandled rejection] Canceled` from Monaco's own inline-suggest debounce
  (`vs/base/common/async.js` `timeout`) when typing cancels it. It is Monaco-internal, only reaches the console
  (the app has no global `unhandledrejection` handler) and does not affect suggestions.
- Consent is turned on from Settings → AI or the status-bar menu. The connection dialog has no AI switch.
- The PostgreSQL schema is not sent (`schema: null`): query documents have no schema of their own; the header
  still lists the database's tables with their schemas.
- Command family (Mongo / Redis): no suggestions until R4b and a command-language prompt.
