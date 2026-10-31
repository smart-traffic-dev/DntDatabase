# R3 — hand-off for R4 and the driver specs

R3 (`docs/superpowers/plans/2026-10-31-r3-connection-config.md`) is done. Mongo/Redis connections can be validated,
parsed from a URI, saved, exported and imported. They cannot be opened yet: no driver is registered.

## Decisions to carry forward

- **Migration 10** rebuilds `connections` with foreign keys off. `prepare()` turns foreign keys off, migrates, then
  turns them on and checks they are on.
  - M10 checks foreign keys over the whole DB and refuses to run with foreign keys on (hook).
  - Any later migration that rebuilds a parent table must do the same.
  - The hook's message names "connections"; generalise it if reused.
- **Stored options:** MySQL/PG `options_json` now carries `"driver":{"kind":"none"}` after the next save. Old rows read
  unchanged.
- **URI parsing:**
  - Low-level parse errors (port, `%` escape, IPv6) keep field `conn_string`; only the new rules use `uri`.
  - Redis `timeout` is read as seconds, and 0 is ignored.
  - Query pairs are applied one at a time for every scheme.
- **`.ncx` skip reasons** for MongoDB/Redis do not repeat the name; the UI renders `<name> — <reason>`. The plan
  document (line ~261) still shows the old format.

## Guards that R4 must replace

- The connection dialog refuses Mongo/Redis in three places. Each one shows
  `MongoDB and Redis connections are not available in this version yet.`
  1. A pasted URI whose scheme is `mongodb://`, `mongodb+srv://`, `redis://` or `rediss://` is refused before parsing.
  2. A parsed result whose engine is Mongo or Redis is refused.
  3. Editing a saved `mongo`/`redis` connection is refused: Save is disabled. Test stays enabled and fails with "no
     driver registered for …". Consider disabling Test as well, or mapping that error to a friendly message.
- `connectionDefaults.engineInfo` falls back to MySQL for unknown kinds (user `root`, port 3306). The R4 dialog needs
  defaults per kind: empty user, 27017 / 6379, and no port for SRV.

## Open for R4 or the driver specs

- **Default SSL mode for new Mongo/Redis connections.** Today the default is `Prefer`, and `command_tls` maps `Prefer`
  to TLS without certificate checks. A plain local Redis/Mongo would then fail out of the box. Default these kinds to
  `Disable`, or show the hint from spec §6.4.
- **Unknown URI options are refused** (spec §6.3). Real Mongo URIs carry `retryWrites`, `w` and similar. Allow a list
  of options that only change driver behaviour, or carry them through to the driver.
- **Gaps in `to_uri`:**
  - It drops `tls=false` and timeouts.
  - `Prefer` reads back as `Require`.
  - IPv6 zone ids are not encoded.
- **`validate_sql` duplicates the text** of the new `require_*` / `validate_ssh` / `validate_ssl` helpers. Merge them
  so the messages cannot drift.
- **Switching the engine while editing** keeps the old driver options. Validation rejects this with field
  `options.driver`, which the dialog has no field for.
