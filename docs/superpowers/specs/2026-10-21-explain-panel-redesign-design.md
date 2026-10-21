# Explain panel redesign — design

Date: 2026-10-21
Status: approved (user: “Okie chốt, làm đi”); v2 approved (“Build Explain v2 as drawn”, canvas
boards 29–30).
Scope: `ui/src/tabs/query`; v2 adds the classic EXPLAIN on the backend (`dnt-query` explain,
`query_sessions` run, `PlanTree.classic` binding).

## v2: Navicat-style views

v1's tree read as hard to follow next to Navicat, whose MySQL Explain opens on the classic
EXPLAIN table everyone knows. v2 keeps everything below and changes the views:

- **View bar `Grid | Tree | JSON`**: flat tabs (`role="tab"`, `aria-selected`), styled like the
  result tabs (flat active tab, no underline: the token rules forbid inset stripes). The third
  tab is `JSON` when `raw` parses as JSON, else `Raw` (MySQL `EXPLAIN ANALYZE` text, 5.7's
  table). MySQL 5.7 has `Grid | Raw`. MySQL / MariaDB open on Grid, PostgreSQL on Tree.
- **Classic table (backend)**: on MySQL 8 and MariaDB, `explain_sql` also returns `classic` =
  `EXPLAIN <stmt>`. The Explain run executes it on the tab's session *before* the plan's own
  statement (plain EXPLAIN never executes the statement, also beside Analyze), with a 10 s
  limit and the run's stop signal; its rows become `PlanTree::Tree.classic: PlanTable`
  (`columns`, `rows` of `string | null`). It streams no events and writes no history. Any
  failure leaves `classic: null` (logged at debug); a lost connection is reported by the plan's
  own statement. MySQL 5.7 (already a table) and PostgreSQL (no such table) get none.
- **Grid with a classic table** (5.7's table plan reads the same way, its `NULL` text as null):
  - summary chips: tables, rows read on the first table, full scans, slow Extra phrases;
  - the server's columns as they are (no upper-casing), a `#` gutter, `(Null)` in grey italic,
    `id` / `key_len` / `rows` / `filtered` right-aligned; `type` tinted good
    (system, const, eq_ref) or slow (index, ALL); Extra's slow phrases tinted (v1 rules);
  - a legend with the access-type ladder `system · const · eq_ref · ref · range · index · ALL`;
  - row click / Enter selects (first row by default); the filter matches any cell;
  - the inspector reads the selected row in plain words: Access (ladder with the row's type
    marked + one sentence), Index (possible keys, key used, a sentence on whether an index
    served it), Rows (examined, and passing = rows × filtered %), Extra (one line per phrase);
  - narrow (< 720 px): the inspector hides and the footer shows the row and its access sentence;
  - after Analyze: a note “Analyzed in X ms. The grid shows the optimizer’s estimates.” with a
    link to Tree, and a dot on the Tree tab.
- **Grid without one** (PostgreSQL, or the classic explain failed): v1's node grid, with the
  overview.
- **Tree**: v1's overview, tree and inspector.
- **JSON**: indented (2 spaces) and coloured: keys accent, strings success, numbers and
  literals warning colour; other text as it came. Copy still copies `raw`.
- Model: `classicPlan.ts` (`classicOf`, `explainRow`, `extraItems`, `accessTone`,
  `classicSummary`, `passRows`); views: `ExplainClassic.tsx`, `ExplainJson.tsx`.

## Goal

Make the Explain tab answer three questions at a glance — how expensive the plan is, which node
is hottest, and how the plan is shaped — while keeping the existing drill-down and Raw output.
Add a dense, sortable **Grid** view in the spirit of Navicat's Explain grid.

Non-goals: a diagram/canvas view, plan export, column chooser, backend changes (no new
`PlanNode` fields). The existing `PlanNode` shape is the whole data source.

## Current state

`ExplainView` renders a collapsible tree with inline metrics, a `Raw` toggle, and (for
`PlanTree::Table`, MySQL 5.7) the engine's columns. The Result panel already supplies the
`Explain` tab; this work is entirely inside that tab.

## Layout

```
Result panel tab bar: Result 1 | Messages | Explain
┌ toolbar ──────────────────────────────────────────────────────────────┐
│ Tree | Grid | Raw    [Filter nodes…]      Expand all  Collapse all  Copy │
├ overview (hidden for table plans) ────────────────────────────────────┤
│ Total cost 12,450 · Rows 1,187 · 419 ms · 7 nodes · ⚠ 1   [cost bar]  │
├ body ───────────────────────────────────────────────┬─ inspector ────┤
│ tree or grid (scrolls)                              │ selected node  │
│                                                     │ properties     │
├ footer (narrow widths only) ────────────────────────┴────────────────┤
│ Selected: Index Scan on orders · 48.2% · 151 ms                       │
└──────────────────────────────────────────────────────────────────────┘
```

- **Tree**: column-aligned rows (`Node | Cost | Self cost | Est. rows | Actual | Time | Loops |
  Warn`), caret + node icon + label + detail. Self cost is a bar scaled to the plan's maximum
  self cost, with the value and share. Bars are `--dnt-accent`; only the hottest node's bar is
  `--dnt-status-warning-solid`, and that row carries a `HOT` pill (no thick accent stripe: the
  token rules allow only 1 px borders). `Actual` is tinted `--dnt-status-warning-fg` when the
  estimate-skew rule fires for the node. `Time` and `Actual` are what the engine reports (per
  loop on MySQL and PostgreSQL, see *Per-loop figures*); the header's `title` says so.
- **Grid**: flat table of the same plan with an `#` gutter and tree guides (`├─`, `└─`) in the
  node column; columns sortable; sticky header; row click selects. Default order is plan order.
- **Raw**: engine output, full width, no inspector or overview.
- **Inspector** (right, 300 px): label + detail; Cost, Rows, Execution, Details and Warnings
  sections. Rows shows estimated and actual (per loop where the engine reports per loop), the
  skew ratio, loops and *Rows in total*; Execution shows *Time per loop* and *Time in total*
  with its share of the root's total time (see *Per-loop figures*). Hidden for Raw/table plans and under 720 px of panel width (container query); the
  narrow footer then shows the selected node summary.
- **Table plans (MySQL 5.7)**: views are Grid and Raw only; Grid shows the engine's own columns
  and rows; no overview/inspector. Grid is the default. Known-slow cell values get the warning
  tint (`--dnt-status-warning-bg` / `-fg`) and a `title` with the reason:
  - `type` = `ALL` → “Full table scan”;
  - `Extra` containing `Using filesort` → “Sorts without an index”;
  - `Extra` containing `Using temporary` → “Uses a temporary table”.
  In `Extra` only the matching phrase is tinted, not the whole cell. Columns are found by
  name, case-insensitive; a plan without those columns gets no tint.

## Overview strip

One 44 px row, only for tree plans with cost data:

- `Total cost` = `root.est_cost` (fallback: sum of self costs).
- `Rows` = `root.actual_rows` when Analyze ran, else `root.est_rows`.
- `Actual time` = `root.actual_ms` (omit when null).
- `Nodes` = node count.
- `Warnings` = total warnings; amber when > 0.
- **Cost distribution bar**: stacked segments of the top 8 nodes by self cost (smaller ones
  lumped into an "Other" segment). Each segment is a button: hover shows “Label — cost (share)”,
  click selects that node. This replaces the three separate “hot node” cards: it carries the
  same information in one line and stays accurate for any plan size.
- Segment colours: the hottest node is `--dnt-status-warning-solid`; the others are
  `--dnt-accent` fading toward `--dnt-bg-app` by rank (`color-mix`, 100 % → 25 %); `Other` is
  `--dnt-border-strong`. No text inside segments (contrast on the faded ones); the `title` and
  `aria-label` carry the label, cost and share.
- Without Analyze: show a `Not analyzed` tag and no time; keep cost/rows estimates, and add a
  **Run Explain Analyze** button (see *Explain Analyze buttons*).

## Per-loop figures

MySQL (tree format) and PostgreSQL report `actual_ms` and `actual_rows` **per loop**; MariaDB
(`ANALYZE FORMAT=JSON`) reports `r_total_time_ms`, already a total. The panel decides from the
tab's connection `ServerInfo.variant`: `MariaDb` → totals; anything else (or unknown) → per
loop.

- `nodeTotals(node, perLoop)` → `{ ms, rows }`: per loop → `actual_ms × loops` and
  `actual_rows × loops` (loops null → 1); totals → the values as given. Null stays null.
- The inspector shows both: *Time per loop* / *Time in total* (≈, 1 decimal) and the share of
  the root's total time; with totals, only *Time in total*. Same for rows.
- The narrow footer shows the total time. The tree and grid columns keep the engine's numbers.

## Explain Analyze buttons

Explain Analyze already exists (`explainQuery(tabId, true)`: it runs the statement, goes through
the normal run flow, so Production asks first; `query_explain` refuses Analyze of a statement
that is not a read). The panel adds two entry points, no new behaviour:

- **Empty state** (no plan yet): “No plan yet for the statement at the cursor.” with
  **Explain** (primary, hint `Ctrl+Shift+E`) and **Explain Analyze** buttons, and the line
  “Analyze runs the statement; on Production it asks first.”
- **Not analyzed** overview: a **Run Explain Analyze** button next to the tag.
- Both Analyze buttons are disabled with the toolbar's title (“Explain Analyze needs MySQL
  8.0.18 or later”) when `analyzeAvailable(info)` is false, and while the tab cannot run
  (the toolbar's `runnable`). Refusals and errors land in the panel as today.

## Derived data (`explainModel.ts`)

Pure functions, unit-tested:

- `flattenPlan(root)` → `{ node, depth, guides, parent }[]` in document order (`guides[i]` marks
  an ancestor that still has a later sibling).
- `selfCosts` (existing `explainTree.ts`) stays the source of self cost.
- `costSegments(root, costs, max = 8)` → segments with `cost`, `share`, optional node; sorted by
  cost desc; `Other` has `node: null`.
- `hotNodes(root, costs, n = 3)` → highest self-cost nodes (used by the bar and tests).
- `planWarnings(root)` → `Map<PlanNode, Insight[]>` and a flat list. Rules, only when the fields
  exist:
  1. **Full scan**: label contains `Seq Scan` and `est_rows >= 10,000`.
  2. **Estimate skew**: Analyze only; `est_rows > 0`, `actual_rows != null`, ratio
     `actual / est >= 5` or `<= 0.2`.
  3. **High loop count**: `loops >= 1,000`.
  Titles/bodies are English, e.g. “Full table scan — Sequential scan over 50,000 rows.”
- Number formatting: grouped integers/floats for costs and rows (`en-US`), 1 decimal for ms,
  shares rounded to 0.1 %. Keep the existing `selfCosts` semantics untouched.

## Interactions

- Selection: defaults to the hottest node; reset when the plan object changes. Click a tree row,
  a grid row, or a cost-bar segment. Selecting outside the tree expands the node's ancestors.
- Expand/Collapse all in Tree; collapsing is per-node and survives a switch to Grid and back.
- Filter: case-insensitive substring over `label` and `detail`. Tree keeps matches plus their
  ancestors (and shows a matching node's subtree); Grid shows matches only; matches are
  highlighted with `<mark>`. Empty result shows “No nodes match”. Clearing restores the view.
- Sorting (Grid): click a header to sort asc/desc, `aria-sort` on the active header; `#` returns
  to plan order. Selection and filter survive.
- Copy: copies `plan.raw` to the clipboard and shows a success toast; failures are silent like
  the app's other copy actions.
- Keyboard: focusable rows (`tabIndex=0`) select on Enter/Space; the caret button keeps
  `aria-expanded`; focus outline follows the token rules.

## Accessibility

- View switcher: `role="group"` with `aria-pressed` buttons; the `Raw` button keeps the exact
  text `Raw` (e2e) and `data-testid="explain-raw"` stays on the `<pre>`.
- The tree uses a list; the grid uses a real `<table>` with `<th scope="col">`.
- Warning pills have `title` text; the warning count has an accessible label.
- Existing test ids preserved: `explain-view`, `explain-node`, `explain-table`, `explain-raw`.

## Styling

All in `ui/src/tabs/query/query.css`, reusing `--dnt-*` tokens (the token-compliance test
forbids raw colours, px font sizes, non-token radii/shadows and thick accent borders). Bars are
`--dnt-accent`, the hottest `--dnt-status-warning-solid` (no amber↔blue mix: it reads as grey).
Rows use
`--dnt-row-h-grid`; the panel is a flex column with a scoped container type for the narrow
rules. Existing `.ex-*` rules are replaced, not layered.

## Files

- `explainModel.ts` (+ `explainModel.test.ts`): flattening, segments, warnings, formatting.
- `ExplainOverview.tsx`: stats + cost bar.
- `ExplainTree.tsx`: column tree, collapse, filter, selection.
- `ExplainGrid.tsx`: sortable grid, table plans, filter, selection.
- `ExplainDetails.tsx`: inspector + narrow footer.
- `ExplainView.tsx`: state (view, filter, selection, collapsed), toolbar, composition.
- `query.css`: `.ex-*` redesign.
- `ExplainView.test.tsx`: updated and extended.

## Testing

- Model unit tests: flatten depth/guides, self-cost clamp, segments/shares, hot nodes, each
  warning rule (including Analyze-only rules), table plans, `nodeTotals` per loop / totals /
  nulls, the 5.7 slow-value matcher.
- Component tests: default hottest selection; tree ↔ grid ↔ raw; sorting; filter and empty
  state; expand/collapse all; warning pills; overview labels; table plan defaults to Grid and
  shows engine columns and tints `ALL` / `Using filesort`; only the hottest bar is amber;
  inspector per-loop and total time (MySQL) vs total only (MariaDB); empty state and Not
  analyzed buttons call `explainQuery(tab, analyze)` and are disabled without Analyze support;
  DDL/error states unchanged; copy writes `raw`.
- Existing e2e selectors (`explain-view`, `Raw`) keep working.

## Rollout

One change set. Run `pnpm --dir ui lint`, `pnpm --dir ui test`, and the web e2e suite. No
migration or settings.
