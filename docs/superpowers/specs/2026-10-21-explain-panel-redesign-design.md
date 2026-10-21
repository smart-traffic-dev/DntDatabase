# Explain panel redesign — design

Date: 2026-10-21
Status: approved (user: “Okie chốt, làm đi”)
Scope: `ui/src/tabs/query` only. No backend or binding changes.

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
  self cost, with the value and share. The hottest node uses the warning background + a `HOT`
  pill (no thick accent stripe: the token rules allow only 1 px borders).
- **Grid**: flat table of the same plan with an `#` gutter and tree guides (`├─`, `└─`) in the
  node column; columns sortable; sticky header; row click selects. Default order is plan order.
- **Raw**: engine output, full width, no inspector or overview.
- **Inspector** (right, 300 px): label + detail; Cost, Rows, Execution, Details and Warnings
  sections. Hidden for Raw/table plans and under 720 px of panel width (container query); the
  narrow footer then shows the selected node summary.
- **Table plans (MySQL 5.7)**: views are Grid and Raw only; Grid shows the engine's own columns
  and rows; no overview/inspector. Grid is the default.

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
- Without Analyze: show `Not analyzed` and no time; keep cost/rows estimates.

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
forbids raw colours, px font sizes, non-token radii/shadows and thick accent borders). Heat uses
`color-mix(in srgb, var(--dnt-status-warning-solid) …, var(--dnt-status-info-solid))`. Rows use
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
  warning rule (including Analyze-only rules), table plans.
- Component tests: default hottest selection; tree ↔ grid ↔ raw; sorting; filter and empty
  state; expand/collapse all; warning pills; overview labels; table plan defaults to Grid and
  shows engine columns; DDL/error/empty states unchanged; copy writes `raw`.
- Existing e2e selectors (`explain-view`, `Raw`) keep working.

## Rollout

One change set. Run `pnpm --dir ui lint`, `pnpm --dir ui test`, and the web e2e suite. No
migration or settings.
