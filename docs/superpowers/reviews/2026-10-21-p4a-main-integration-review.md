# P4a: main integration review and desktop smoke

Date: 2026-10-21. Branch: `feat/p4a-object-actions`.

## Baseline

P4a started at `a21d293` and was saved in `18eea24`. Main `5d75886` was merged in `c117507`; main `3c350a5` was merged in `54030b9`. The latter only adds the Explain redesign spec. Main `7f5e5c9` was then merged in `a2bcdd7` (menu placement and autocomplete CSS); the UI suite and native smoke were rebuilt and rerun against that baseline. The main worktree and its uncommitted changes were preserved. No CI changes.

## Conflicts resolved

| Issue | Resolution |
|---|---|
| P4a changed the flat tabs array; main now has editor groups, groupOf and per-group active tabs | Add replaceObjectTabs inside the store so settle preserves group/focus/preview/pin invariants |
| Truncate keeps the canonical tab id, allowing old resources to survive | Invalidate metadata/page/request tokens/selection/edit state and increment resourceRevision to remount DataView |
| Main removed query-tab HistoryView | Keep it removed; show object_action origin in the new sidebar HistoryPanel |
| Main moved notices to a toast stack | Preserve the stack and add objectActionRunning for the native close guard |
| Main moved object header actions into the tab strip | Preserve that layout and retain the P4a unknown-outcome recovery |
| Modal structure and theme tokens changed | Use modal-body/modal-foot and current dnt tokens; CSS variable/compliance tests pass |
| Smoke relied on damaged menu labels, the result-summary node and closing a busy dialog | Repair labels; inspect Messages for DDL/DML completion; wait for Close to become clickable and the dialog to disappear after refresh |

Direct merge conflicts were dialogs.ts, ObjectTab.tsx, test/backend.ts, and modify/delete HistoryView.tsx. No unmerged entries remain. Main has no competing backend migration: P4a adds migration 5 after migration 4, with preservation tests for history id/origin/outcome_unknown.

## Verification after integration

| Check | Result |
|---|---|
| Full UI Vitest | 122 files, 1,339 tests passed |
| UI lint and TypeScript | Passed |
| UI production build | Passed; existing bundle-size warning remains |
| Desktop E2E TypeScript | Passed |
| Rust fmt check | Passed |
| Rust clippy, workspace/all targets, warnings denied | Passed |
| P4a integration with DNT_TEST_DB=1 | Two tests passed: lifecycle/dependency matrix and real metadata-lock cancellation |
| Native P4a desktop smoke, WebView2 155 | Both MySQL 8 and PostgreSQL 17 scenarios passed |

The integration matrix actually ran against MySQL 5.7/8.0, MariaDB 10.6/11.4, and PostgreSQL 12/17. These are not skipped DB-test results. Cancellation was additionally exercised during a real metadata-lock wait on MySQL 8 and PostgreSQL 17.

Desktop smoke follows real UI and IPC: create a private namespace, create/insert a table through SQL, rename, truncate, drop the table, and drop the namespace. Raw SQL uses explicit F5 tree refresh; P4a actions refresh automatically. Tests wait until Apply and refresh complete before closing the dialog.

Evidence: `e2e/evidence/2026-10-21T08-13-29/` (gitignored). The SQL-preview/typed-confirmation screenshot was visually inspected on the merged UI. After the passing run, independent server queries found no p4a_smoke_% namespaces on MySQL 8 or PostgreSQL 17. The runner removed its test credentials. Cleanup only targets UUID namespaces created by the test.

## Specific regressions covered

- Rename in a background group preserves that group and its preview, without changing the focused query.
- Drop removes an emptied group and retains the surviving query group.
- Truncate preserves pin/group/active identity while changing the resource revision.
- Unknown outcomes hide cached data and require explicit verification, with no automatic retry.
- Native close cannot exit while an object action is running.

## Limits

This desktop run covers P4a, not every pre-existing desktop spec. Raw SQL DDL still needs manual tree refresh. MySQL/MariaDB conservatively report Unknown after a sent DDL fails, rather than promising rollback. Metadata rechecks cannot eliminate races with external sessions.

The main worktree also contains uncommitted Explain redesign files. Their changed paths and ex-* CSS scope were inspected and do not overlap the P4a changes. They were preserved and are not included in the tested binary; the tested baseline is committed main 7f5e5c9.
