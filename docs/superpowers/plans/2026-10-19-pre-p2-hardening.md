# Pre-P2 hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task.

**Goal:** Close the safety items that the P1 final review marked "before P2", so the query editor can be built on a trustworthy execute path.

**Architecture:** These are local fixes in existing modules. There are no new crates and no UI work.

**Spec:** `docs/superpowers/specs/2026-10-19-p1-core-design.md`. The sources are `2026-10-19-p1-core-final-review.md` (Recommendations 5, triage of T3/T10/T12, ruling caveat T18) and `2026-10-19-p1-core-backlog.md`.

**Branch:** `fix/pre-p2-hardening`, cut from `feat/p1-core` at 68ddf94.

## Global Constraints

- Secrets never go to SQLite, logs, `Debug` output or frontend payloads.
- Every task must keep `cargo test --workspace` green, both with and without `DNT_TEST_DB=1` (the Docker matrix is up on the ports in `testdata/docker-compose.test.yml`).
- Every task must keep `pnpm --dir ui test` green.
- `CARGO_HOME=D:\dev\cargo` and `RUSTUP_HOME=D:\dev\rustup`, with `D:\dev\cargo\bin` on PATH.
- Never kill whatever holds port 5173.
- Commits end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. No pushes.
- Text files are UTF-8 without BOM.

## Review Focus

- A PG script containing `E'..\'..'`, `'C:\'` or `$tag$ … ; … $tag$` must classify and split exactly as the PG server parses it.
- A statement that commits during the cancel grace period, or just before a timeout fires, must never be reported to the user as "not applied".
- Concurrent Cancel clicks and a timeout racing a cancel must leave the session usable, with correct `in_tx` state.
- Editing a connection and pressing Test must never send a stored password to a network path different from the one it was saved for.

---

### Task 1: `same_target` compares the whole network path (logic → Opus)

**Files:** `src-tauri/src/session_manager.rs:236` (`same_target`) and its tests (`src-tauri/tests/session_manager.rs` or the in-file tests).

1. `same_target(stored, cfg)` returns true only when all of the following are equal: engine; DB host (case-insensitive, trimmed), port and user; SSH enabled flag; and, when SSH is enabled, SSH host, port and user.
2. When it returns false, `test` does not fall back to stored secrets for that kind. A missing password then surfaces as the existing `password_required` kind.
3. Add tests that pin each case:
   - `ssh_on_to_off_is_different_target`
   - `ssh_off_to_on_is_different_target`
   - `ssh_host_changed_is_different_target`
   - `ssh_user_changed_is_different_target`
   - `unchanged_ssh_is_same_target`
   - `db_host_case_only_change_is_same_target`
4. Ledger: mark the "Final: parked — same_target…" backlog line as resolved.

### Task 2: dialect-aware `classify` (logic → Opus)

**Files:** `crates/core/src/sql/classify.rs`, and its callers `crates/driver-mysql/src/session.rs:328` and `crates/driver-pg/src/session.rs:344`.

1. Change the signature to `classify(stmt: &str, dialect: Dialect) -> StatementKind`, reusing `Dialect` from `sql::split`.
   - Its lexer must follow the same per-dialect rules that `split` already uses.
   - **PG:**
     - Backslash is not an escape in `'…'` (standard_conforming_strings on).
     - `E'…'` honours backslash escapes.
     - `$tag$…$tag$` is opaque.
     - Backticks are ordinary characters.
     - There are no `#` comments and no `/*! */` executable comments.
   - **MySQL:** the current rules.
2. Fix the parenthesised main query after a CTE: `WITH x AS (…) (SELECT …)` → `Read`.
3. Keep all existing MySQL assertions passing under `Dialect::MySql`.
4. Add a `pg_dialect` test module with these cases:
   - `WITH a AS (SELECT 'C:\') DELETE FROM t` → Write
   - `SELECT E'it\'s'` → Read
   - `WITH a AS (SELECT $$ ) $$) UPDATE t SET x=1` → Write
   - `SELECT '#' ` → Read
   - `/*! DELETE FROM t */` under PG → Other (it is a plain comment there)
5. Also add the multibyte split tests from backlog T3: `'ư;đ'`, `'\ư'` (MySQL), and `$thẻ$ ; $thẻ$` (PG).

### Task 3: MySQL execute path: cancel, timeout and commit reporting (logic → Opus)

**Files:** `crates/driver-mysql/src/{session.rs,cancel.rs}` and the driver tests.

1. **Concurrent cancels:** the cancelled-generation write uses `fetch_max` (`cancel.rs:85`), so a stale cancel can never lower it.
2. **Deadlock and cancel ordering:** on error 1213, clear `in_tx` before the cancel check (`session.rs:376-380`), so a coinciding cancel cannot skip it.
3. **Grace period:** a statement that finishes successfully during the cancel/timeout grace period reports its real result, not `Timeout`/`Cancelled`.
4. **Timeout on a write:** when a timeout fires for a `Write`/`Ddl` statement and the server outcome is unknown (the kill was sent but the statement's completion was not observed), return `DbError::Timeout` with a message stating that the statement may have been applied.
   - **Ruling:** reuse the existing `Timeout` variant and add an `outcome_unknown: bool` field. The `AppError` mapping puts the wording in `message`.
   - Regenerate the UI bindings and fixtures with the existing generator test, if the shape changes.
5. **Stuck-timeout replacement:** if the replacement connection fails after a stuck timeout, abort the old task and return `ConnectionLost { was_in_tx }`. Also add the `message` field missing from `ConnectionLost`.
6. **`USE db` after a leading comment:** track it by classifying and inspecting with comments stripped.
7. **Integration tests (DNT_TEST_DB):**
   - `cancel_twice_keeps_session_usable`
   - `write_finishing_in_grace_reports_success`: use `DO SLEEP(x)` plus an insert, or a lock wait.
   - `use_after_comment_tracks_db`
   - A unit test for `fetch_max` ordering.

### Task 4: PG execute path: same fixes plus type output (logic → Opus)

**Files:** `crates/driver-pg/src/{session.rs,values.rs}` and the driver tests.

1. **Mirror Task 3 items 3–5** for PG:
   - grace-period success;
   - `Timeout{outcome_unknown}` for Write/Ddl;
   - stuck replacement → `ConnectionLost{was_in_tx, message}`.
   - Consume Task 3's error-shape change. Do not redefine it.
2. **`SET search_path` after a leading comment:** track it.
3. **Unmapped types:** `inet`, `cidr`, `macaddr`, `macaddr8`, `money`, `timetz`, `tsvector`, `interval`, `bit`/`varbit` and `xml` render as their PG text form, not hex.
   - **Ruling:** decode the binary wire format for each of these into PG's text form when it is fixed and documented (inet/cidr/macaddr/macaddr8/money/timetz/bit/interval). Fall back to a `::text`-equivalent result only where decoding is impractical (tsvector). Hex stays only for truly unknown OIDs.
4. **BC timestamps:** render as PG text does (`0044-03-15 12:00:00 BC`), not as an invalid ISO string.
5. **Tests:** extend the `all_types_roundtrip` snapshot with the types above, plus `grace_success`, `search_path_after_comment` and a BC timestamp.

### Final

Run a whole-branch review on Opus, then a full test run (Rust with DB, UI, E2E smoke), then hand back to the user.
