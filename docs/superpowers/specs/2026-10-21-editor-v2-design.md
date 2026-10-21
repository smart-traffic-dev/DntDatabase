# Editor v2: VS Code-style groups, minimal toolbar, right sidebar

Date: 2026-10-21. Canvas boards 23–26 (row "Editor groups v2 · VS Code-style"), approved by the user.
Builds on `2026-10-21-ux-redesign-design.md` (§3 shell, §4 editor groups, §5 query tab). Where they
differ, this document wins. All UI copy is English; tokens are `--dnt-*` only; focus is 1px.

## 1. Split tree and drag & drop (board 23)

- **Model.** The fixed presets (`single | columns | rows | grid`) give way to a split tree with at most
  4 leaves: `type Layout = { kind: 'group'; id } | { kind: 'split'; dir: 'row' | 'col'; ratio: number; a; b }`
  (binary nodes; `ratio` = share of `a`, clamped so each side keeps ≥ 320 px wide / 200 px tall at
  render time, and 0.15–0.85 in the store). `groups`, `groupOf`, `focusedGroupId`, the per-group
  active tab and the derived `activeId` stay as they are.
- **Split.** `splitGroup(groupId, side: 'left' | 'right' | 'up' | 'down', tabId?)` replaces that leaf by a
  split node holding the old group and a new one on `side`, ratio 0.5. Only that leaf changes; the
  other groups keep their sizes. With 4 groups nothing splits. Existing `splitTab(tabId, 'right' | 'down')`
  becomes a wrapper (split the tab's own group, move the tab into the new group).
- **Closing a group** removes its leaf; its sibling takes the parent's place (and size).
- **Layout presets** stay as commands (View menu, title bar picker, Ctrl+K 1–4) and build the
  matching tree: single, row(1,2), col(1,2), col(row(1,2), row(3,4)).
- **Resize.** Every split node renders one splitter (8 px hit area, 1 px line, accent on hover);
  drag sets that node's ratio only; double-click sets it to 0.5. Arrow keys on a focused splitter move it.
- **Drop zones on a group:** an overlay above the whole pane (editor and grid included, so Monaco and
  the canvas grid never receive the drag) appears on `dragenter` of a tab or a tree object. Outer
  third on each side = split that side; the rest = move into the group. The zone that will become
  the new group is tinted (`accent-subtle` + 1 px accent border) with a label ("Split left", "Move here").
  Edge zones only when a split is possible (fewer than 4 groups, and not a group's only tab over
  its own group); otherwise the whole pane means Move.
- **Tab strip:** a 2 px accent insertion bar shows where the tab lands, in its own strip or another
  group's. Pinned tabs stay first (the bar never shows inside the pinned block for an unpinned tab).
  Esc cancels a drag.
- The moved tab keeps its state (text, results, running query, transaction), becomes active, and its
  group takes the keyboard focus. A group left empty closes.
- **Restore:** the tree (with ratios) and each group's tabs are saved and restored; an older saved
  arrangement (preset + ratios) is converted on load.

## 2. Minimal toolbar in the tab strip (board 24)

- The 40 px query toolbar row and the object tab's header row go away. Each group's tab strip ends
  with an **action cluster** for its active tab (icon buttons, 26 px, aria-label + tooltip with shortcut):
  - **Query tab:** context chip `[ENV] database` (connection, server, env in the tooltip; click opens
    today's connection / database picker; narrow group: badge only), Run ▶ with ▾ menu (Run statement,
    Run all, Run selection, Continue on error, Add to favorites), Explain, ⋯ (Format, Limit radio list,
    Auto / Manual commit, Open in new group). Production: Run icon in `status-danger`, badge PROD.
    Running: Stop ■ in the same element and slot, elapsed time beside it. Open transaction: amber
    "Tx mm:ss" chip + Commit and Rollback icons, only while open.
  - **Object tab:** context chip, Filter (F3), Refresh, ⋯ (Export, Open in query, Page size, Count rows).
    The sub-tabs (Data, Structure, DDL, Info) and the pager share one 28 px row under the strip.
- The strip's tabs shrink and scroll before the cluster does; under 360 px the cluster keeps the chip
  badge, Run / Stop and ⋯.
- Existing testids stay on the controls that survive (`query-run`, `query-stop`, `query-context`, …).

## 3. Right sidebar (board 25)

- A 40 px activity bar on the window's right edge with icon buttons: History (enabled), Snippets,
  Object info, AI assistant (disabled, "Coming soon"). Clicking an icon opens its panel; clicking the
  active one collapses it. The panel is resizable (260–560 px, remembered) and its state persists.
- Ctrl+Alt+B toggles the panel; Ctrl+Shift+H opens History and focuses its search. View menu: "Right
  sidebar" (checked) and "History".
- **History panel** (app-wide, replaces the per-tab History tab): search box, connection filter
  (default: the focused tab's connection; "All connections"), status segment All / OK / Errors,
  entries grouped Today / Yesterday / Older with status dot, time, duration, rows / error, database.
  Click expands the full SQL with Insert (at the focused editor's caret), Open in new tab, Run again
  (normal run guards apply, Production asks again), Copy; double-click = Insert; context menu = the
  same plus Delete. Clear all keeps its confirmation.
- Query results panels keep Result N and Messages only.

## 4. Extras (board 26)

1. **Preview tab:** single click on an object in the tree opens it as the group's preview tab (title
   in italic); opening another object replaces it. Double-click, editing, sorting, filtering or
   pinning makes it a normal tab. At most one preview tab per group.
2. **Maximize group:** double-click a tab, Ctrl+K Ctrl+M, or the group menu: the focused group fills
   the editor area; a "Maximized" marker with a restore button shows in its strip; restoring puts the
   tree back unchanged. Moving focus to another group (Ctrl+Alt+N) restores first.
3. **Maximize results:** a toggle on the results bar (and Ctrl+Shift+J) makes the results fill the
   group, the editor folding away; toggling back restores the ratio.
4. **Run state on the tab:** spinner while running; a green / red dot when a run ends on a tab that is
   not visible (cleared when shown); amber dot for an open transaction. Each mark has aria-label and
   tooltip.
5. **Results per group:** results visibility and the editor / results ratio are kept per group;
   Ctrl+J acts on the focused group; a new tab in a group takes that group's setting.
6. **Drag from the tree:** an object dragged from the connection tree onto a group uses the same drop
   zones (edge = open in a new group there, middle = open in that group).

## 5. Testing

Unit tests for the tree operations (split each side, close and collapse, ratios, presets, restore and
migration), drop-zone choice, insertion index, preview-tab replacement, maximize / restore, per-group
results, history filters and actions. e2e-web: drag a tab onto each edge and onto another strip,
resize a splitter, preview tab, right sidebar open / collapse and Insert from History, minimal
toolbar in a narrow group. Each change: `pnpm --dir ui lint`, `pnpm --dir ui test`, `pnpm e2e:web`.
