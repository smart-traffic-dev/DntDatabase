# F1 — hand-off for F2 (app backend, Settings → AI)

F1 (`docs/superpowers/plans/2026-11-04-f1-ai-crate.md`) adds `dnt_query::redact_spans` and the pure crate
`crates/ai` (`dnt-ai`). Nothing in `src-tauri` or `ui` uses it yet.

## What F2 builds on

- **Flow per suggestion:** `prompt::build(&PromptInput)` → `FimRequest::new(model, p.prompt, p.suffix)` →
  `HttpFimClient::complete` → `post::clean(&reply.text, &PostCtx { statement_head: &p.statement_head,
  prefix_tail: p.prefix_tail(), suffix: &p.suffix, dialect, max_lines })`. Keep the `FimPrompt` until the reply
  arrives: `clean` needs it.
- **`Skip::InSecret`** means "no suggestion", silently. It is not an error.
- **Cancel:** run each `complete` in a task and abort it (or drop the future). The fake-server test
  `drop_cancels_the_request` proves the socket closes. The client holds no per-request state.
- **One client per settings value:** `HttpFimClient` is `Clone` and pools connections; build a new one when the
  base URL, key or timeout changes, never per request.
- **Errors that pause auto-suggest** (spec §7): `Unauthorized`, `FimUnsupported`, `BadRequest` (until settings
  change); `RateLimited` (for `retry_after`, default 30 s). `Timeout`, `Network`, `Server`, `InvalidResponse` drop
  the one suggestion; count 3 in a row for the error dot. `Config` and `Redirected` are settings problems: show
  them in Settings → AI.
- **Consent origin:** `Endpoint::origin()` (`scheme://host[:port]`, default port omitted).
- **Settings → Test:** `complete` with `PROBE_PROMPT` / `PROBE_SUFFIX`, then `judge_probe(&reply.text)`.
  `IgnoresSuffix` is a warning, not a failure.
- **Schema input:** `TableBrief { schema, name, columns: Vec<ColumnBrief> }` maps 1:1 from `SchemaCacheTable`
  (`cache.rs`); `ObjectKind` is not needed.
- **Cursor:** `PromptInput::cursor_utf16` is Monaco's model offset as is.

## Choices made in F1 that differ from or refine the plan

- `Endpoint::parse` also refuses user info in the base URL (`https://u:p@host`), so a key never hides there.
- `post::clean` trims trailing whitespace of every suggestion, and skips the repetition check for suggestions
  shorter than 3 characters (a lone `)` that also ends the prefix is usually real).
- `judge_probe` checks `ORDER BY` / a final `;` on the **raw** reply: `clean` removes an echoed suffix, which
  would otherwise hide that the model ignores it.
- `AiError::Network` messages are built from reqwest's error chain with the URL removed (`without_url`).
- The real-endpoint smoke test is `cargo test -p dnt-ai --test provider -- --ignored` with `DNT_AI_BASE_URL`,
  `DNT_AI_MODEL` and optional `DNT_AI_KEY`. Against `https://api.openai.com/v1` without a key it reports
  `The API key was rejected (401).`, which shows the ring + OS-certificate-store TLS path works on Windows.

## API notes found during F1

- reqwest 0.13: `ClientBuilder::tls_backend_preconfigured(rustls::ClientConfig)` (`use_preconfigured_tls` is the
  deprecated 0.12 name). With `rustls-no-provider`, reqwest has no crypto provider of its own; the config must
  carry one.
- rustls-platform-verifier 0.7: `BuilderVerifierExt::with_platform_verifier()` on
  `ClientConfig::builder_with_provider(ring).with_safe_default_protocol_versions()?` uses that builder's provider.
- `cargo tree -i aws-lc-rs` / `native-tls` / `openssl-sys`: no match.

## Open items

- `SecretStore` still keys secrets by connection; F2 adds app-level secrets for `dnt.ai.default`.
- Ghost text for MySQL scripts with a custom `DELIMITER`: `post::clean` only ends a statement at `;`.
