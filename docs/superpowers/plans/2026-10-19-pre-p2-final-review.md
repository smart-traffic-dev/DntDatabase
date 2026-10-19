# Pre-P2 hardening: final whole-branch review (68ddf94..3c0fbe8)

The review was done by Opus and is read-only. Its verdict was **With fixes**.

## Strengths

- **One error shape.** `Timeout`, `Cancelled` and `ConnectionLost` all carry `outcome_unknown`, share a single "may have been applied" wording, and use one retryable rule in `error.rs`.
- **Drivers mirror each other.** The MySQL and PG drivers share the same structure: `stuck_error`, `reset_notes`, `session_custom` and `fetch_max` cancel generations. Where they differ, the cause is the database engine itself (PG statement atomicity versus MySQL MyISAM/DDL/`CALL` behaviour).
- **Split and classify agree.** `classify` and `strip_comments` reuse `split`'s lexer, so they cannot disagree.
- **Secrets stay put.** `same_target` and `same_ssh_endpoint` gate the use of stored secrets per kind.
- **New decoders are panic-free.** None of the new decoders can panic on input, and no BOMs or debug leftovers were found.
- **Earlier I3 ruling verified.** An Overwrite import closes the runtimes it replaces.

## Issues and disposition

| Id | Severity | Issue | Disposition |
|---|---|---|---|
| I1 | Important | MySQL reports a lost connection with a pending cancel as `Cancelled`. It also checks for cancel too early after a reconnect. | Fixed in the final fix wave |
| I2 | Important | MySQL maps interrupts it did not cause (another session's KILL, `max_execution_time` 3024, MariaDB 1969) to `Cancelled`. | Fixed in the final fix wave |
| I3 | Important | `AppError` lacks `outcome_unknown`, and `isCancelled` would hide "may have been applied". The spec §4.4/§7 examples are stale. | Fixed in the final fix wave |
| I4 | Important (P2) | A PG timeout inside a transaction is marked retryable even though the transaction is aborted. | Prerequisite for P2 |
| M1 | Minor | A late PG timeout SIGINT after a grace-period success can hit the next statement in the script. | P2 |
| M2 | Minor | A MySQL write on a connection the server dropped while idle (2006/4031) is reported as outcome unknown. | Fixed in the final fix wave |
| M3 | Minor | `/*!40101 SET … */` is not detected as a session-state change. | P2 |
| M4 | Minor | MySQL `killed_outcome_unknown` returns false for a killed COMMIT. | Fixed in the final fix wave |
| M5 | Minor | Word splitting differs between the drivers for non-ASCII text. | P2 |
| M6 | Minor | Stuck-statement tests rely on a fixed 10 s grace period. P2 needs a "stopping…" state in the UX. | P2 |

## P2 prerequisites carried forward

- I4, M1, M3, M5 and M6 from the table above.
- The Production confirmation should consider confirming `Other` statements too, for example MySQL `PURGE BINARY LOGS`, `FLUSH`, `RESET` and `KILL`, which are still classified as Other.
- MySQL `NO_BACKSLASH_ESCAPES` and `ANSI_QUOTES` are not modelled.
- The PG session has no "transaction aborted" state; the user only finds out from the 25P02 error on the next statement.
