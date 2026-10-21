# UX/UI review and Navicat-parity pass

Date: 2026-10-20 (status updated 2026-10-21, after the port onto `main`)
Scope: `ui/src` (Tauri 2 + React desktop client)
Status: batch 1 was written on `fix/pre-p2-hardening` (commit 96005b1, never merged). On 2026-10-21 it was
ported onto `main` (branch `feat/ux-port`): where `main` had meanwhile built the same capability its own way,
`main`'s version was kept and only the missing behaviour was ported. The tables below say, per item, what is on
`main` now and how it got there.

## Method

- Walked every persistent surface: toolbar, sidebar/tree, tab strip, object tab,
  query tab (toolbar, editor, results, messages, history, explain), status bar,
  modal host and toasts.
- Reviewed the daily Navicat flows the app is meant to replace: connect, browse,
  open several objects, run/edit SQL, inspect and export results, manage
  transactions, recover from errors, use the app without a mouse.
- Checked behaviour against the existing specs (`p1-core`, `p2-query`, design
  tokens) and kept their contracts.
- This batch is UI/UX and front-end behaviour only.

## Problems found in batch 1, and where they stand on `main`

"main" = already built on `main` by its own work (menu bar, command palette, result grid tools, P3 data editing…);
"port" = brought over from batch 1 by the 2026-10-21 port; "open" = still not on `main`.

| # | Area | Problem before | Batch 1 change | On `main` now |
|---|------|----------------|----------------|---------------|
| 1 | Window layout | Sidebar and editor/result split were fixed. | Persisted layout store and an accessible `Splitter`. | **main.** `store/layout.ts` (sidebar 180–600 px, editor ratio, sidebar/results visibility, Reset layout) and `components/Splitter.tsx`. |
| 2 | Tab strip | No scrolling affordance, no bulk close, no keyboard model, no new-query button. | Scrollable strip, `New query` button, roving `tabIndex`, arrows/Home/End, `Shift+F10`, tab menu (Close, Close others, Close all, Copy title, Save), middle-click close. | **main + port.** main: active tab scrolled into view, wheel scrolling, list-all-tabs menu, middle-click close, Close / Close others / Close to the right, Pin, Reopen closed tab. **Port:** one tab stop with arrows / Home / End, context-menu key and `Shift+F10`, Close all (unpinned), Copy title, Save on a query tab. New query stays in the toolbar, File menu and `Ctrl+T` (no strip button). |
| 3 | Menus | Action, tree and tab menus were separate implementations. | One `Menu` component (clamping, focus, arrows, Escape, focus return, separators, disabled/danger). | **main + port.** main's `ActionMenu` already does all of this (tree, toolbar, menu bar, favorites, cell menu). **Port:** the tab strip's separate `TabMenu` was removed; its menus now use `ActionMenu` (`checkRole="radio"` for the open-tabs list). The batch's `Menu.tsx` was not added. |
| 4 | Connection tree | Expanded empty nodes looked broken; filter gave no count; empty workspace gave no next step; selection could yank scroll. | `empty` placeholder rows, empty-tree CTA, filter match count + Clear, Escape clears, scroll only on selection change. | **main + port.** main: no-connections call to action in the sidebar, "no objects match" and per-folder "matches / total" while filtering, "Searching loaded objects" hint. **Port:** placeholder rows ("No tables", "No databases", "Empty group"…) and no scroll yank (scrolls when the selection changes or the selected row first appears). Not ported: a clear button / Escape handler for the filter box (it is a native search field; WebView2 already clears it with Escape and its × button). |
| 5 | Result grid | Could not answer "which row am I on", "find this value", "read this long cell", "export what I see". | Client-side row filter and sort, fit/resizable columns, long-cell viewer, cell menu, Save CSV, auto-dismissing notices. | **main.** Find (`Ctrl+F`), go to row (`Ctrl+G`), selection statistics, record view, value editor, read-only overlay for long cells, cell menu (copy / with headers / as INSERT / as CSV / column name), fitted and resizable columns, export dialog (CSV, JSON, Excel, SQL, Markdown), auto-dismissing notices; server-side filter and sort on the Data tab. Client-side filter/sort of query results was **not** ported (see "Not ported, open decisions"). |
| 6 | Status bar | No cursor context, no run progress, no stop action, misleading charset. | `Ln/Col · N selected`, `Running… n s` + Stop, connection charset, read-only badge. | **main.** Cursor and selection, running counter with statement progress, stopping / abandoning state, read-only badge, transaction state with Commit / Rollback. Stop lives in the query toolbar, Query menu and `Esc`. Not ported: the charset label (the connection's `charset` option is not applied by the drivers, so showing it as "the connection character set" would be wrong). |
| 7 | Query toolbar | Database list only appeared after expanding the tree; disabled actions gave no reason. | Fetch databases of open connections; tooltips for disabled states. | **main + port.** main: disabled-reason tooltips on Run / Run all / Run selection / Explain. **Port:** the toolbar lists the databases of an open connection itself (merged with the tree's), the Database picker says why it is short on a closed connection, the Explain caret has the same disabled reason. |
| 8 | Object tab | DDL hard to read/save, refresh gave no feedback, structure headers scrolled away. | Line count, wrap, Save DDL, `Refreshing…`, sticky headers. | **main + port.** main: DDL in a read-only Monaco editor (line numbers, highlighting), Open in editor, Copy. **Port:** Refresh shows `Refreshing…` and is disabled while loading, DDL Wrap toggle, sticky structure headers. Save DDL not ported (Open in editor, then Save). |
| 9 | Modals and toasts | Only footer actions closed dialogs; repeated toasts reused a stale timer. | Close X on every dialog; toast dismiss and timer restart. | **main + port.** main: opt-in close X (`Modal closable`). **Port:** notice toast dismiss button and timer restart for a repeated notice. The always-on X was not ported. |
| 10 | Empty workspace | "No object open" with no action. | "Start working" with New query / New connection. | **main + port.** main: `EmptyWorkspace` (first-start New connection, else New query and the run keys). **Port:** the idle, empty Messages panel names the run keys. |
| 11 | Shortcuts | Only run, save, open and new query. | `Ctrl+W`, `Ctrl+Tab`, `Ctrl+Shift+Tab`, `Ctrl+PageUp/PageDown`. | **main.** All of these plus `Ctrl+Shift+T`, `Ctrl+1..9`, `Ctrl+P`, `Ctrl+Shift+P`, `Ctrl+B`, `Ctrl+J`, `Ctrl+E`, `F1` and more. |
| 12 | Tests | New behaviour had no regression coverage. | Unit tests for the above. | **main + port.** Every ported behaviour has unit tests; the browser E2E harness (`e2e-web`, `pnpm e2e:web`) was ported too. |

### Not ported, open decisions

Batch 1 behaviours that differ from what `main` chose; `main`'s behaviour was kept:

- **Client-side filter and sort of query results** (header click sorts, a filter box hides rows). `main` has
  Find (`Ctrl+F`) in query results and server-side filter/sort on the Data tab; a client-side sort only orders
  the loaded rows of a truncated result.
- **Borderless surface restyle** (batch 1's `theme.css`, `dialogs.css`, `query.css`, `objectTab.css`: tabs,
  panels, dialogs and tables separated by fills instead of 1 px borders). A visual-design direction, not a fix.
- **A close X on every dialog.** `main` shows it only where a dialog opts in (`closable`).
- **A `+` (new query) button in the tab strip.** `main` has New query in the toolbar, File menu and `Ctrl+T`.
- **Stop in the status bar.** `main` has Stop in the query toolbar, the Query menu and `Esc`.

## Remaining findings, by severity

### P0 — blocks using the app as a daily Navicat replacement

1. **No data editing.** — **Resolved on `main`** (P3): Data tab editing with
   staged inserts/updates/deletes, Apply/Discard with preview, row-version and
   snapshot protection, manual commit, value editor, copy as INSERT/UPDATE.
2. **No Data tab for tables/views.** — **Resolved on `main`**: Data tab with
   paging, a filter bar and sorting pushed to the server.
3. **No object creation/alter UI.** — **Open.** The toolbar's Tables, Views,
   Functions, Import/Export, Backup, ER diagram and Compare are still disabled
   placeholders. DDL can be viewed, copied and opened in a query editor, but there
   is no table designer.
4. **No destructive-statement guard.** — **Resolved on `main`**: `UPDATE` /
   `DELETE` without `WHERE` (and always-true forms) is detected and confirmed on
   every connection (setting "Confirm UPDATE/DELETE without WHERE").

### P1 — high-friction gaps for regular use

5. **Object tabs are not restored.** — **Open.** Query tabs survive a restart;
   table/view tabs do not. (Closed tabs can be reopened within a session with
   `Ctrl+Shift+T`.)
6. **No quick open / command palette.** — **Resolved on `main`**: `Ctrl+P`
   quick open over the objects of every connection that the tree or autocomplete
   has loaded (it does not query servers), and `Ctrl+Shift+P` command palette.
7. **Result actions are still partial.** — **Mostly resolved on `main`**: copy as
   TSV / with headers / INSERT / CSV with shortcuts, export to CSV, JSON, Excel,
   SQL and Markdown. Still open: column hide / pin / reorder.
8. **No tab reorder or tear-off.** — **Partly open.** Tabs can be pinned; they
   cannot be reordered or torn off.
9. **Close many tabs stops at the first cancelled guard** without a summary. —
   **Open** (unchanged on `main`; Close others / to the right / all behave so).
10. **No visible keyboard map.** — **Resolved on `main`**: Help → Keyboard
    shortcuts (`F1`), and shortcuts in the menu bar and command palette.

### P2 — polish and consistency

11. **Settings are minimal.** — **Partly resolved on `main`**: data page size,
    idle-transaction warning, unbounded-write confirmation, theme, debug logging;
    code font zoom (`Ctrl+=` / `Ctrl+-`), Reset layout. Still missing: tab width,
    row height, default row limit.
12. **Result canvas accessibility.** — **Partly resolved**: the record view shows a
    row as text; there is still no full accessible table view.
13. **Explain menu is hand-rolled** in `QueryToolbar.tsx`. — **Open** (its keyboard
    model matches `ActionMenu` and is covered by tests).
14. **No per-connection session restore** (last database/schema, expanded nodes). —
    **Open.**
15. **No data import UI** (CSV/SQL). — **Open.** (Connection import/export exists;
    the toolbar's Import/Export is a placeholder.)
16. **No unsaved-changes indicator on the window title/taskbar.** — **Open** (the
    title only reports a run that finished in the background).

## Navicat parity matrix (on `main`, 2026-10-21)

| Capability | State | Notes |
|---|---|---|
| Connection manager, groups, SSH, env tags | Have | Duplicate, drag between groups, reconnect, import/export |
| Object tree with lazy loading and filter | Have | Empty/error/retry states, empty-node rows |
| Object tabs (Data, Info, DDL, Structure) | Have | DDL view / copy / open in editor |
| Table data browse/edit | Have | P3: staged edits, Apply/Discard, manual commit |
| Query editor with run selection, explain, history | Have | Results, messages, history, explain |
| Transaction status/commit/rollback | Have | Auto / Manual commit mode |
| Result filter/sort/export | Have | Server-side filter/sort on the Data tab; find in query results; export in 5 formats |
| Table/view/function designer | Missing | P0 |
| Import/export (CSV/SQL/JSON) | Partial | Export yes; data import no |
| ER diagram, compare, backup | Missing | Placeholders |
| Multi-tab workspace, restore | Partial | Query tabs restore; object tabs do not; pin, reopen closed |
| Keyboard-first operation | Have | Menu bar, command palette, quick open, shortcuts dialog |
| Dark/light/system themes | Have | Token-based |

## Suggested next batches

1. **Designer (P0):** apply-DDL flow, table designer for columns/indexes/FKs.
2. **Workspace continuity (P1):** restore object tabs and last database, tab
   reorder, a summary when a bulk close stops at a guard.
3. **Import (P2):** CSV/SQL data import.
4. **Polish (P2):** column hide/pin/reorder, more editor/grid settings,
   window-title dirty marker, accessible table view for results.

## Verification (port, 2026-10-21)

```powershell
pnpm --dir ui lint
pnpm --dir ui test
pnpm e2e:web
```

Result: `eslint src && tsc --noEmit` clean; 96 test files, 1092 tests passing; browser E2E 3 passed.
