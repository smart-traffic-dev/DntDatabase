# P4c — Edit and apply DDL for views, routines, triggers and events

- Date: 2026-10-25
- Status: written by the P4c stream lead without user review (the user was not available); every decision
  the framework spec left open is recorded as a ruling in §10.
- Basis: roadmap P3–P6 §2 (principles), §4.2 (N2 plans), §4.3 (N3 guard), §6.4; P4a spec (object actions,
  `ddl_exec`), P4b spec (designer, single-use plans, `PlanStale`, review dialog).
- Branch: `feat/p4c-ddl-edit` from `main` at 2a55cbb. Runs in parallel with P5 (import/dump/restore, jobs)
  and P6 (admin, schema compare, ER diagram): P4c code lives in new modules, shared hotspots get small
  appended additions only, and there is **no store migration** (§8, ruling R1).

## 1. Goal

From the DDL section of an object tab (or the connection tree), the user edits the definition of a view,
materialized view, function, procedure, trigger or event as SQL text, reviews the exact statements the app
will send, and applies them. "New …" in the tree opens the same editor on a per-engine template. Every apply
goes through the P4a/P4b machinery: a single-use backend plan, a byte-exact preview, the backend guard, a
re-check of the object on the server before the first statement, an explicit outcome, History, and a tree /
SchemaCache refresh.

Local-first like P4a/P4b: no CI, no cloud, no external service.

## 2. Scope

| Kind | MySQL 5.7 / 8.0 | MariaDB 10.6 / 11.4 | PostgreSQL 12 / 17 |
|---|---|---|---|
| View | edit: `CREATE OR REPLACE VIEW` (one statement); new | same | edit: `CREATE OR REPLACE VIEW`, or drop + create; new |
| Materialized view | — | — | edit: drop + create (+ its indexes); new |
| Function | edit: drop + create, rehearsed (§5.2); new | edit: `CREATE OR REPLACE FUNCTION`; new | edit: `CREATE OR REPLACE`, or drop + create; new |
| Procedure | as Function | as Function | as Function |
| Trigger | edit: drop + create; new | edit: `CREATE OR REPLACE TRIGGER`; new | edit: drop + create; new |
| Event | edit: drop + create; new | edit: `CREATE OR REPLACE EVENT`; new | — (no events) |

Out of scope: renaming an object through its DDL (the name, schema and kind of the edited text must stay the
same: ruling R4), editing tables (P4b designer), sequences and types, overloaded PostgreSQL routines
(several `pg_proc` rows with the same name: edit is refused with a reason, R9), a PostgreSQL trigger name used
on several tables (refused, R9), comments and grants re-application after a drop + create (warned, R11),
text diff view, server-side syntax checks without executing.

## 3. User flow

1. **Edit**: object tab → DDL section → **Edit** (or tree context menu **Edit definition**) opens a DDL editor
   tab ("Edit · {name}") holding the server's current DDL, exactly as the DDL section shows it. Tree menu
   **New view… / New function… / New procedure… / New trigger… / New event… / New materialized view…** on the
   matching folder (and **New trigger…** on a table) opens the editor on the engine's template.
2. The tab header shows `{Kind} · {name}`, `connection › database[.schema]` and the engine, a strategy switch
   when there is a choice (PostgreSQL view / function / procedure: **Replace** or **Drop and re-create**), the
   notes for this engine and kind (e.g. "MySQL has no CREATE OR REPLACE PROCEDURE …"), **Revert** and
   **Review & apply…**. The body is an editable Monaco editor in the connection's SQL dialect.
3. **Review & apply…** asks the backend for a preview: the backend validates the text (one statement that
   defines exactly this object, §5.1), builds the statements (§5.2), re-reads the object (stale check), judges
   every statement with the guard, stores a single-use plan and returns it. The dialog shows the statements
   byte for byte, the warnings (drop gap in danger colour), whether it runs in one transaction, the checks run
   before `COMMIT`, and the confirmation (Production: type the object name; Staging when the plan has a
   non-transactional drop gap: type the name; otherwise Apply).
4. **Apply** sends exactly the stored statements. The dialog shows the outcome, which statements ran, the
   object's state after a failure, and — when a MySQL drop ran but the create failed — **Put the original
   definition in the editor** (the restore flow, §6.3).
5. After an apply that sent anything: tree and SchemaCache refresh, completion cache invalidated, the object's
   open object tab reloads its DDL, and on success the editor's text becomes the server's DDL as re-read
   (clean, not dirty).

Closing a dirty DDL editor tab, closing its connection's tabs, importing over its connection or quitting asks
first (the `discardEdits` question used by staged edits and designs).

## 4. Architecture

### 4.1 Pure logic — `crates/core/src/sql/ddl_edit/` (new module `dnt_core::sql::ddl_edit`)

No database access; every branch unit-tested.

```rust
/// What the edited text must define and where.
pub struct EditTarget {
    pub kind: ObjectKind,            // View | MaterializedView | Function | Procedure | Trigger | Event
    pub database: String,
    pub schema: Option<String>,      // PostgreSQL only
    pub name: Option<String>,        // None: a new object
    pub table: Option<String>,       // a trigger's table (edit: from the catalog; new: optional hint)
}
pub enum Strategy { Replace, Recreate }           // serde snake_case, specta
pub struct Header {                               // the parsed CREATE header
    pub kind: ObjectKind, pub or_replace: bool,
    pub qualifier: Option<String>, pub name: String,
    pub create_end: usize,                        // byte offset just past the CREATE keyword
    pub name_span: (usize, usize),                // byte span of the (qualified) name
    pub on_table: Option<(Option<String>, String)>, // triggers: the ON target
}
pub fn editable_kind(kind: ObjectKind, server: &ServerInfo) -> Result<(), DbError>;
pub fn isolate(text: &str, kind: ObjectKind, dialect: Dialect, server: &ServerInfo) -> Result<String, DbError>;
pub fn parse_header(stmt: &str, dialect: Dialect) -> Result<Header, DbError>;
pub fn check_target(h: &Header, t: &EditTarget, server: &ServerInfo) -> Result<(), DbError>;
pub fn build(input: &BuildInput) -> Result<Built, DbError>;
pub fn template(t: &EditTarget, server: &ServerInfo) -> String;
pub fn strategies(t: &EditTarget, server: &ServerInfo) -> Vec<Strategy>;
```

- `isolate` returns the one statement of the text (trimmed, without its delimiter) or a `Config { field:
  "text" }` error saying why not (§5.1).
- `build` takes the isolated statement, its header, the target, the strategy, the server and catalog facts
  (§4.2) and returns `Built { statements: Vec<String>, warnings: Vec<String>, transactional: bool, gap: bool,
  checks: Vec<PostCheck> }`. `PostCheck { sql: String, params: Vec<String>, expect: String, failure: String }`
  is a PostgreSQL read run inside the transaction before `COMMIT`; its single text value must equal `expect`
  or the transaction is rolled back with `failure` as the error.
- Identifiers are quoted with `core::sql::ident`; nothing from the UI except the text and the strategy is used.

### 4.2 Backend — `src-tauri/src/ddl_edit/` (new module)

- `catalog.rs`: parameterised catalog reads on the edit's session (`ExecOptions::params`, PostgreSQL `$n`,
  MySQL `?`): PostgreSQL routine rows by schema + name (count, `prokind`, identity arguments, has comment,
  has ACL), relation facts (oid, `relkind`, has comment, has ACL, materialized-view index definitions via
  `pg_get_indexdef`), trigger rows by schema + name (count, table name, table oid, has comment); MySQL trigger
  facts (table, timing, event, number of other triggers with the same timing and event on that table).
- `mod.rs`: `DdlEditStore { edits, plans, operations: DdlOperations }`, `ddl_edit_open`, `ddl_edit_preview`,
  `ddl_edit_close`, `ddl_edit_cancel`, fingerprint, context checks.
- `apply.rs`: `ddl_edit_apply`, outcomes (§6), History, refresh.
- `commands/ddl_edit.rs`: thin `#[tauri::command] #[specta::specta]` wrappers, registered at the end of the
  `collect_commands!` list.

| Command | Does |
|---|---|
| `ddl_edit_open(connection_id, scope, kind, name: Option<String>, table: Option<String>) -> DdlEditOpen` | Reads the object's DDL (`Introspector::object_ddl`, as the DDL section) and catalog facts on a query session, or builds the template for a new object; stores the edit (base text, fingerprint, dialect, server, target). A read-only connection opens too (view only). |
| `ddl_edit_preview(edit_id, text, strategy) -> DdlEditPreview` | §5: isolate, parse, check target, re-read (stale), build, guard, store plan. |
| `ddl_edit_apply(plan_id, confirmed, name, operation_id, channel) -> DdlEditReport` | §6. |
| `ddl_edit_cancel(operation_id)` | Stop, like `table_design_cancel`. |
| `ddl_edit_close(edit_id)` | Forget the edit and its plans. |

```rust
pub struct DdlEditOpen {
    pub edit_id: Uuid, pub text: String, pub target: EditTarget, pub object: Option<ObjectRef>,
    pub server: ServerInfo, pub env: Option<EnvTag>, pub read_only: bool,
    pub strategies: Vec<Strategy>, pub strategy: Option<Strategy>,
    pub blocked_reason: Option<String>,   // edit not possible (read-only, overloads, DDL not shown)
    pub notes: Vec<String>,               // engine/kind notes shown in the header
}
pub struct DdlEditPreview {
    pub plan_id: Uuid, pub statements: Vec<String>, pub warnings: Vec<String>, pub checks: Vec<String>,
    pub transactional: bool, pub gap: bool, pub confirm_name: Option<String>, pub expires_in_seconds: u32,
}
pub enum EditOutcome { Applied, NotApplied, RolledBack, Partial, Unknown }
pub enum ObjectState { Unchanged, Changed, Missing, Unknown }
pub struct DdlEditReport {
    pub outcome: EditOutcome, pub error: Option<AppError>, pub warnings: Vec<String>,
    pub statements_sent: u32, pub executed: Vec<String>, pub failed: Option<String>,
    pub object_state: ObjectState, pub restore_text: Option<String>,
    pub object: Option<ObjectRef>, pub text: Option<String>,
}
```

`SessionManager` gains one field `ddl_edits: crate::ddl_edit::DdlEditStore`, and closing a connection drops
its edits and plans and cancels its applies (one line next to `design_cancel_connection`).

### 4.3 UI

- Tab kind `'ddl'` (`DdlTabRef { id, kind: 'ddl', connectionId, objectKind, title, pinned?, preview? }`, id
  `ddl/<uuid>`); the edit target lives in the new store `useDdlEdits` (`ui/src/store/ddlEdits.ts`), so a create
  that succeeds turns the tab into the edit of the created object without re-keying it. Opening the editor of
  an object that already has a DDL tab activates that tab.
- `ui/src/tabs/ddl/DdlEditorTab.tsx` (header + editable Monaco), `ui/src/dialogs/DdlEditDialog.tsx` (review &
  apply), entry points in `ObjectTab`'s DDL panel and `TreeContextMenu`.
- Every `tab.kind` switch gets the new kind (EditorArea, TabBar label "DDL" + dirty dot, TabStripActions,
  tabActions close + unsaved + connection/import guards, tabs `closeConnection`, StatusBar, HistoryPanel,
  resultExport); `queryTabs` gets a second drafts source for quitting; `dialogs` gets `ddlEdit` request and
  `ddlEditRunning`.
- PostgreSQL trees gain a **Triggers** folder (the driver lists each trigger name once, R9).
- All UI copy in English.

## 5. Preview

### 5.1 Validating the text (`isolate`, `parse_header`, `check_target`)

1. Text larger than 1 MiB, empty, or (MySQL) holding an executable comment `/*! … */` is refused.
2. **One statement.**
   - MySQL family: `DELIMITER` lines are honoured (`core::sql::split`); without them the whole text is one
     candidate. A view must be one `split` statement that does not `resplit`. A function, procedure, trigger
     or event is checked by the **body scanner**: tokens are walked with a block depth (`BEGIN` and `CASE`
     open; `END` and `END CASE` close; `END IF / LOOP / WHILE / REPEAT` are neutral; a `BEGIN`/`END` in an
     identifier position — after `.`, `,`, `(`, `=`, `SELECT`, `AS`, or before `,`, `)`, `.`, `=`, `FROM`,
     `AS` — is a name, not a block word). The statement ends at the first depth-0 `;`; anything but blanks,
     comments and `;` after it, a negative depth or a depth left open refuses the text ("Only one statement
     can be applied; … found after the end of the body"). MySQL runs a multi-statement text piece by piece
     (multi-statements are on), so this check is what keeps a pasted second statement from running.
   - PostgreSQL: `split` must give one statement; a text holding `BEGIN ATOMIC` (SQL-standard body, PG 14+)
     is taken whole, since the server receives it through the extended protocol, which refuses a second
     command anyway. `BEGIN ATOMIC` on PostgreSQL 12/13 is refused as unsupported.
   - A trailing `;` is dropped; leading comments stay.
3. **Header.** `CREATE [OR REPLACE] [ALGORITHM = …] [DEFINER = …] [SQL SECURITY …] [AGGREGATE] [CONSTRAINT]
   [RECURSIVE] [MATERIALIZED] VIEW | FUNCTION | PROCEDURE | TRIGGER | EVENT <name>`. Refused: `IF NOT EXISTS`
   (would hide an existing object), `TEMP`/`TEMPORARY`, MySQL loadable functions (`… SONAME`), anything not
   starting with `CREATE`. Names: backtick (MySQL) or double-quote (PostgreSQL, MySQL with `ANSI_QUOTES`)
   identifiers are unquoted; unquoted PostgreSQL names fold to ASCII lower case.
4. **Same object** (`check_target`): the text's kind equals the target's kind; the name equals the object's
   name exactly; the qualifier, when given, equals the database (MySQL) / schema (PostgreSQL); PostgreSQL
   requires the qualifier (the session's `search_path` would otherwise decide the schema). A trigger's `ON`
   table must be the trigger's table (PostgreSQL: unqualified allowed, then verified by a post-check). A new
   object's name passes `validate_identifier`, and the object must not exist yet.
5. **Engine and version.** MySQL (not MariaDB) has no `CREATE OR REPLACE` for functions, procedures, triggers
   and events: such text is refused as unsupported ("write CREATE …; the app drops and re-creates it").
   PostgreSQL has no `CREATE OR REPLACE MATERIALIZED VIEW`; `CREATE OR REPLACE TRIGGER` needs PostgreSQL 14.
   A new object's text must not say `OR REPLACE`.

### 5.2 Statements (`build`)

`q(x)` = the object's qualified, quoted name. PostgreSQL statements always run inside one `BEGIN … COMMIT`
added by the executor (not listed in the preview, which says "Runs in one transaction").

| Engine · kind · strategy | Statements | Gap | Post-checks (PostgreSQL) |
|---|---|---|---|
| MySQL/MariaDB view, edit | text with `OR REPLACE` inserted after `CREATE` if absent | no | — |
| MySQL function/procedure, edit | 1. text with the name replaced by `` `_dnt_p4c_check_<8 hex>` `` (rehearsal) · 2. `DROP {KIND} <rehearsal>` · 3. `DROP {KIND} q(x)` · 4. text | between 3 and 4 | — |
| MySQL trigger, edit | `DROP TRIGGER q(x)` · text | yes | — |
| MySQL event, edit | `DROP EVENT q(x)` · text | yes | — |
| MariaDB function/procedure/trigger/event, edit | text with `OR REPLACE` inserted | no | — |
| PostgreSQL view/function/procedure, Replace | text with `OR REPLACE` inserted if absent | no | routine: exactly one `pg_proc` row with this name, of this kind; view: relation is a view |
| PostgreSQL view, Recreate | `DROP VIEW q(x) RESTRICT` · text | no (transaction) | relation is a view |
| PostgreSQL function/procedure, Recreate | `DROP {KIND} q(x)(<identity args>) RESTRICT` · text | no | exactly one `pg_proc` row, of this kind |
| PostgreSQL materialized view, edit | `DROP MATERIALIZED VIEW q(x) RESTRICT` · text · every former index definition | no | relation is a materialized view |
| PostgreSQL trigger, edit | `DROP TRIGGER "t" ON q(table)` · text | no | trigger `t` exists on the original table's oid |
| any, new | text | no | the object exists in the target schema |

Warnings (concrete where the catalog knows): the MySQL gap ("Between DROP and CREATE the procedure does not
exist; calls fail" / "writes to `orders` do not fire this trigger" / "a scheduled run may be missed"); MySQL
trigger order ("other BEFORE INSERT triggers exist on `orders`; the re-created trigger fires after them");
drop + create loses the comment and the grants it has (PostgreSQL; MySQL routines: "privileges granted on the
routine may be revoked"); materialized view refill ("filled again WITH DATA; holds an exclusive lock while it
runs"); dependants make a RESTRICT drop fail and roll back; each lock wait is limited to 5 seconds.

### 5.3 Stale check, guard, confirmation, plan

- Fingerprint = hash of the object's DDL text and its catalog facts (edit), or `"absent"` plus the absence
  of the name among the kinds sharing its namespace (new). It is taken at open; preview re-reads it and a
  difference is `PlanStale { reason: "The object changed since the editor opened it." }`; apply re-reads it
  on the DDL session before the first statement (`PlanStale`, nothing sent).
- Guard: a read-only connection is `Blocked` (preview and apply); every statement is judged with
  `dnt_query::verdict(stmt, classify(stmt), dialect, env, read_only)`; a `Block` is `Blocked`. The guard can
  only raise protection.
- Confirmation (`confirm_name` = the object name as the backend has it, no trim): Production always; Staging
  when the plan has a gap; Local / untagged: click Apply.
- Plan: statements, checks, target, fingerprint, connection runtime + generation, env, dialect, expiry
  (`plan_ttl`), consumed once (also on failure). `MAX_PLANS` 128, 64 open edits.
- `check_ddl_idle` (no open transaction on a query tab of the connection) as P4a/P4b.

## 6. Apply

### 6.1 Common

As `table_design_apply`: take the plan, register the operation id (pre-cancel honoured), re-check context
(connection config, env, read-only, runtime generation, dialect, guard, idle), check the confirmation, open a
dedicated session on the object's database (MySQL: `USE` via the connect database, so unqualified names in
server DDL resolve), `SET lock_timeout = '5s'` / `SET SESSION lock_wait_timeout = 5`, re-read the
fingerprint, send the stored statements in order with progress `executing i/n`, never re-render.

### 6.2 PostgreSQL

`BEGIN`, `SET LOCAL standard_conforming_strings = on` (as the designer; keeps the server's lexing equal to
the app's), the statements, each post-check (a mismatch rolls back with its message), `COMMIT`. Outcomes via
`ddl_exec::failure_outcome`: `RolledBack` only when the server confirmed `ROLLBACK`; a lost `COMMIT` is
`Unknown`; a failure before the first plan statement is `NotApplied`. `object_state` = `Changed` on success,
`Unchanged` after a confirmed rollback or `NotApplied`, `Unknown` otherwise. `executed` / `failed` are only
filled for `Partial` (MySQL).

### 6.3 MySQL / MariaDB (not transactional)

Statements run one by one, stopping at the first failure. After a failure the app re-reads the object (same
session after a server error, a fresh session after a transport loss, bounded by `METADATA_CALL_TIMEOUT`):

| Re-read | `object_state` | `outcome` |
|---|---|---|
| fingerprint equals the one at open | `Unchanged` | `NotApplied` (a left-over rehearsal object is named in a warning with its `DROP`) |
| object absent | `Missing` | `Partial`; `restore_text` = the original DDL |
| present but different | `Changed` | `Partial` |
| cannot read / transport lost | `Unknown` | `Unknown` |

`executed` lists the statements that completed, `failed` the one that failed. With `Missing`, the backend
turns the edit into a **new** edit of the same name (base = absent), so the restore is an ordinary create:
the dialog puts `restore_text` in the editor, and the user reviews and applies it (guard and confirmation
included). Nothing is retried automatically.

### 6.4 After any apply that sent a statement

- History: one entry, origin `object_action` (R1), SQL = the statements; for MySQL compound statements the
  entry is written as `DELIMITER $$ … $$ DELIMITER ;` so it can be re-run from a query tab; redacted as usual;
  `outcome_unknown` for `Unknown`. A History failure adds `HISTORY_FAILED` to the warnings.
- `after_ddl(conn, database)`: tree, SchemaCache, edit plans, loaded values.
- Success: the edit's base becomes the re-read DDL (returned as `text`) with a new fingerprint; a new object
  becomes an edit of the created object (`object`). A failed re-read after success adds a warning; the outcome
  stays `Applied`.

## 7. UI states

| State | UI |
|---|---|
| `blocked_reason` | Banner with the reason; editor read-only; Review disabled |
| Validation error at preview | The backend message in the dialog (e.g. "The text defines view `sales`.`v2`, not `sales`.`v`") |
| `Unsupported` | Reason + minimum version |
| `PlanStale` | "The object changed since you opened it." + **Reopen** (reloads the base from the server, keeps the user's text) |
| `PlanExpired` | **Preview again** |
| Applied | Toast "Changes applied.", editor text = server DDL, object tab DDL reloads |
| NotApplied / RolledBack | Server message, text kept |
| Partial | Executed / failed statements, object state; Missing → **Put the original definition in the editor** |
| Unknown | "The result is unknown; check the object before continuing." + object tabs marked for verification |
| Lost session (connection closed) | Text kept, banner + **Reopen** |

## 8. History origin and the store

A dedicated History origin (`ddl_edit`) needs a store migration: `query_history.origin` has a `CHECK (origin
IN (…))` constraint that P4b rebuilt the table to extend (MIGRATION_6). The P4c brief forbids store
migrations in this stream (P5 adds migrations in parallel). P4c therefore records its applies with the
existing origin `object_action` (R1). The follow-up — `HistoryOrigin::DdlEdit` ↔ `"ddl_edit"`, a migration
rebuilding `query_history` with the extended CHECK, the History panel label "DDL edit", and switching
`ddl_edit::HISTORY_ORIGIN` — is written down in the final review for whoever sequences the migrations.

## 9. Tests and completion

1. Core unit tests: `isolate` (DELIMITER, body scanner incl. labels, `CASE` expressions and statements,
   handlers, `END IF`, refusals of a second statement, unbalanced bodies, executable comments), `parse_header`
   (every kind, quoting, ANSI_QUOTES, PG folding, refusals), `check_target`, `build` per row of §5.2,
   `template` round-trips through `isolate` + `parse_header` + `check_target`.
2. Backend unit tests on a fake session: read-only blocked, stale → zero statements, single-use plan,
   Production / Staging-gap typed names, PG failure → RolledBack, post-check failure → RolledBack, MySQL
   failure after DROP → Partial + Missing + restore_text + edit becomes new, History origin and DELIMITER
   form, lock bounds sent first, cancel.
3. Integration (`DNT_TEST_DB=1`, `Target::all()` = MySQL 5.7, 8.0, MariaDB 10.6, 11.4, PostgreSQL 12, 17;
   scratch namespace `p4c_<uuid>`, dropped afterwards): edit + apply every kind the server has; each template
   created as-is; PG view column change via Recreate, function signature change refused by the post-check and
   rolled back; MV indexes re-created; MySQL trigger whose CREATE fails after DROP → Partial/Missing, then the
   restore re-creates it; MySQL procedure with a syntax error → NotApplied by the rehearsal, procedure intact;
   DELIMITER-wrapped text; concurrent change → PlanStale with nothing sent.
4. UI: store (dirty, revert, lost, stale keep-text, applied, create → edit), tree menu entries, DDL panel Edit
   button, close guard, dialog (typed names, outcomes, restore, stop).
5. Browser E2E: edit/apply a view, Production typed confirmation, stale → reopen, MySQL partial failure →
   restore offer, new object from a folder, dirty close asks.
6. fmt, clippy, Rust tests, UI lint/tests, e2e-web typecheck + suite; results in the final review. A skipped
   DB test does not count as verified.

Done when every kind of §2 can be edited and created on each server that has it, the preview equals the
statements sent byte for byte, no text with a second statement runs, every plan passes the guard, Production
asks the name, nothing is retried, and Partial / Unknown reach the UI and History as such.

## 10. Rulings

| # | Ruling | Cost if wrong |
|---|---|---|
| R1 | History uses origin `object_action`; a `ddl_edit` origin is a follow-up needing a store migration (§8). | One enum variant + one migration later; P4c entries cannot be filtered apart until then. |
| R2 | A new tab kind `'ddl'` with a store-held target (random tab id), not an editable mode of the object tab: "New …" has no object tab, and the designer set the precedent. | A few more `tab.kind` switches. |
| R3 | MySQL/MariaDB views use `CREATE OR REPLACE VIEW` (atomic) instead of drop + create; the framework's "MySQL: drop + create" applies to routines, triggers and events, and on MariaDB those use `CREATE OR REPLACE` too. | None known; both are single statements. |
| R4 | No renames through DDL editing: name, qualifier and kind must match; a different name is refused. | Users rename with a query tab. |
| R5 | MySQL functions/procedures are rehearsed under a temporary name before the drop, so syntax, DEFINER and binlog-safety errors fail before anything is dropped. Triggers and events are not rehearsed (a rehearsal trigger would fire on live writes; a rehearsal event could run). | Two extra statements in the preview; a temporary routine is briefly visible. |
| R6 | The MySQL body scanner refuses rather than guesses; an unquoted identifier named `BEGIN`/`END` in an unusual position may be refused (quote it). The residual risk (a crafted text that balances a mis-counted block) is accepted: the same user can run any SQL in a query tab through the same guard. | False refusals in rare texts. |
| R7 | PostgreSQL routine edits verify inside the transaction that exactly one routine of that name and kind exists afterwards, so a changed argument list never leaves a second overload; the user switches to Drop and re-create to change a signature. | One read before COMMIT. |
| R8 | PostgreSQL requires the schema-qualified name in the text (server DDL always has it); trigger `ON` tables may stay unqualified in edits and are verified by oid before COMMIT. New triggers need a qualified `ON` table. | Users add a qualifier to hand-written text. |
| R9 | PostgreSQL overloaded routines and trigger names used on several tables are not editable (blocked reason). The PostgreSQL tree gains a Triggers folder listing each name once. | Those objects need a query tab. |
| R10 | Confirmation: Production types the name for every apply; Staging types it when the plan has a non-transactional drop gap; Local/untagged click. | One more typed name on Staging MySQL routine/trigger/event edits. |
| R11 | Comments and grants are not re-applied after a drop + create; the preview warns with what the catalog shows. Materialized-view indexes are re-created (their loss would silently change performance). | Users re-grant by hand. |
| R12 | After a MySQL failure the object is re-read and its state reported; `Missing` turns the edit into a create of the same name with the original DDL offered, never applied automatically. | None. |
| R13 | `IF NOT EXISTS` and `OR REPLACE` in new-object text are refused; existence is checked by the app (P4a rule: no `IF [NOT] EXISTS` to mask concurrent changes). | Users remove the clause. |
| R14 | Templates avoid syntax newer than the oldest server in the matrix (no CTEs or window functions on MySQL 5.7; `EXECUTE FUNCTION` is PG 11+); new MySQL events are created `DISABLE`d; functions are declared `DETERMINISTIC` (binary logging on MySQL 8). | Templates are plain. |
