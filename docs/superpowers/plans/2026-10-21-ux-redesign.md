# UX/UI Redesign Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the redesign in six shippable phases; this plan details phase 1 (design tokens, base rules, density) and outlines phases 2–6.

**Architecture:** Phase 1 rewrites `ui/src/theme/tokens.css` as a three-layer `--dnt-*` token set and keeps every existing variable name as an alias, so the whole app re-tints without component changes. Base interaction rules (focus, disabled, selection) are centralised in `theme.css`; the canvas grid and Monaco read the new semantic tokens; density is a `data-density` attribute driven by ThemeProvider.

**Tech Stack:** React 18 + TypeScript, plain CSS custom properties, Vitest + Testing Library, Glide Data Grid, Monaco, Playwright (e2e-web).

**Spec:** `docs/superpowers/specs/2026-10-21-ux-redesign-design.md` (canvas boards 01–08, 20–22).

## Global Constraints

- All UI copy is English.
- Accent `#2563D9` light / `#6B9BF2` dark; chrome neutral grey.
- Focus: no outline wider than 1 px, no ring, no glow; bordered controls turn their 1 px border to `--dnt-focus`; other elements use `outline: 1px solid var(--dnt-focus); outline-offset: -1px`.
- Disabled: `color: var(--dnt-fg-disabled)`, never `opacity`.
- Active tab: flat, no rounded top, no coloured top bar (already enforced by `tokenCompliance.test.ts` "no accent bars").
- Density default Compact; Comfortable via setting.
- Existing variable names keep resolving until phase 6 removes them.
- No new dependencies.

## Review Focus

1. Dark mode after the re-tint: every semantic token must have a `.dark` value; a missing one silently falls back to the light value. → Task 1 test `every semantic token is defined for light and dark`.
2. Low-contrast captions: `fg-muted` / `fg-subtle` on `bg-chrome` must stay ≥ 4.5:1 in both themes. → Task 1 test `secondary text meets 4.5:1 on chrome`.
3. Keyboard focus disappearing where the old rules said `outline: none` (tree, menu items, options) → Task 2 test `focus is visible on rows and menu items` (CSS check that those selectors get the 1 px accent outline, not none).
4. Grid / editor keeping old colours after a theme switch (they read variables at runtime) → Task 3 test `grid theme follows the dark class`.
5. Density surviving a restart and not breaking when storage is unavailable → Task 4 tests.

---

## Phase 1 — Tokens, base rules, density

### Task 1: Three-layer token file with legacy aliases

**Files:**
- Modify: `ui/src/theme/tokens.css` (full rewrite)
- Create: `ui/src/theme/tokens.test.ts`

**Interfaces:**
- Produces (CSS custom properties, all prefixed `--dnt-`):
  - Primitives: `gray-0 #ffffff`, `gray-25 #fafafa`, `gray-50 #f4f4f5`, `gray-100 #ebebed`, `gray-200 #dedee1`, `gray-300 #c6c6ca`, `gray-400 #9d9da3`, `gray-500 #707076`, `gray-600 #5a5a60`, `gray-700 #3f3f44`, `gray-800 #2a2a2e`, `gray-850 #222225`, `gray-900 #1b1b1e`, `gray-950 #131315`; `blue-50 #eef3fe`, `blue-100 #dce7fd`, `blue-400 #6b9bf2`, `blue-500 #3b78e7`, `blue-600 #2563d9`, `blue-700 #1d4fb0`; `red-50 #fdeeee`, `red-200 #f5c2c2`, `red-500 #dc3b3b`, `red-700 #b42318`; `amber-50 #fff6e0`, `amber-200 #f6d98c`, `amber-500 #d99a00`, `amber-700 #7a4b00`; `green-50 #eaf7ef`, `green-200 #b5e2c6`, `green-600 #1a7f46`.
  - Semantic, light (`:root`) / dark (`.dark`):

    | token | light | dark |
    |---|---|---|
    | bg-app | gray-0 | gray-900 |
    | bg-chrome | gray-25 | gray-950 |
    | bg-sunken | gray-50 | #161618 |
    | bg-raised | gray-0 | #26262a |
    | bg-hover | rgba(19,19,21,.05) | rgba(255,255,255,.06) |
    | bg-active | rgba(19,19,21,.09) | rgba(255,255,255,.10) |
    | bg-selected | blue-100 | rgba(59,120,231,.28) |
    | bg-selected-muted | gray-100 | gray-800 |
    | border-subtle / border / border-strong | gray-100 / gray-200 / gray-300 | #242427 / #2f2f33 / #45454b |
    | fg / fg-muted / fg-subtle / fg-disabled | gray-950 / gray-600 / gray-500 / gray-400 | #ececee / #b3b3b8 / #8e8e94 / gray-600 |
    | accent / accent-hover / on-accent / accent-subtle | blue-600 / blue-700 / #fff / blue-50 | blue-400 / #8ab0f5 / #0b1830 / rgba(59,120,231,.18) |
    | focus | blue-500 | blue-400 |
    | status-danger-{fg,bg,border,solid} | red-700, red-50, red-200, red-500 | #ff8a8a, #3a2224, #5a2f31, #e5484d |
    | status-warning-{…} | amber-700, amber-50, amber-200, amber-500 | #f0c674, #362c17, #5a4618, #e0a82e |
    | status-success-{…} | green-600, green-50, green-200, #23a05a | #5fd08f, #1b3326, #2a5a3c, #2fb36a |
    | status-info-{…} | blue-700, blue-50, blue-100, blue-600 | #8ab0f5, #1a2540, #2a3b63, blue-400 |
    | env-prod / env-staging / env-local | status-danger-solid / status-warning-solid / gray-400 | same tokens / gray-600 |
    | shadow-1 / shadow-2 / shadow-3 | `0 1px 2px rgba(0,0,0,.06)` / `0 6px 20px rgba(0,0,0,.10), 0 1px 3px rgba(0,0,0,.06)` / `0 20px 48px rgba(0,0,0,.18), 0 2px 6px rgba(0,0,0,.08)` | ×2.5 alpha |
    | scrim | rgba(19,19,21,.32) | rgba(0,0,0,.55) |

  - Component (compact on `:root`, comfortable on `[data-density='comfortable']`): `ctl-h-xs 20/22`, `ctl-h-sm 24/26`, `ctl-h-md 28/30`, `ctl-h-lg 32/36`, `row-h-tree 24/28`, `row-h-grid 24/30`, `row-h-menu 28/32`, `titlebar-h 36/40`, `tab-h 34/38`, `statusbar-h 24/26`; fixed: `icon-size 16px`, `space-0_5 2px`, `space-1 4px`, `space-1_5 6px`, `space-2 8px`, `space-3 12px`, `space-4 16px`, `space-6 24px`, `space-8 32px`, `radius-xs 3px`, `radius-sm 5px`, `radius-md 7px`, `radius-lg 10px`, `radius-full 999px`, `text-xs 11px/16px`, `text-sm 12px/16px`, `text-base 13px/18px`, `text-md 14px/20px`, `text-lg 16px/22px`, `text-xl 20px/28px`, `text-2xl 28px/34px` (as `--dnt-text-X` and `--dnt-text-X-lh`), `dur-1 80ms`, `dur-2 140ms`, `dur-3 220ms`, `ease-std cubic-bezier(.2,0,0,1)`, `ease-exit cubic-bezier(.4,0,1,1)`, `z-sticky 10`, `z-dropdown 100`, `z-overlay 200`, `z-modal 300`, `z-toast 400`, `z-tooltip 500`.
  - Legacy names kept as aliases (both themes, via the semantic tokens): every `--studio-*` (`bg→bg-app`, `sidebar→bg-chrome`, `hover→bg-hover`, `selected→bg-selected`, `hairline→border`, `control→bg-app`, `subtle→bg-sunken`, `bubble→bg-sunken`, `fg/fg-secondary/fg-tertiary→fg/fg-muted/fg-subtle`, `accent/accent-fg/accent-hover→accent/on-accent/accent-hover`, `danger/danger-fill/danger-border→status-danger-solid/-bg/-border`, `composer*→bg-app/border/bg-hover`, `send-disabled*→bg-active/fg-disabled`, `popover→bg-raised`, `mention→accent-subtle`, `scrollbar→border-strong`, `shadow→shadow-2`), the legacy block (`--bg`, `--text`, …) and the `--color-*` block unchanged in name. Old type and radius names (`--text-caption` … `--radius-xl`) map onto the new scale: caption→xs, footnote→sm, callout→base, body→md, headline→lg, title→xl, large-title→2xl; radius sm→`radius-sm`, md→`radius-md`, lg/xl→`radius-lg`. `--font-sans` / `--font-mono` unchanged.
  - Base rules kept in this file: `::selection`, `::placeholder`, scrollbar, reduced-motion. The `:focus-visible` rules move out (Task 2).

- [ ] **Step 1: Write the failing tests** in `ui/src/theme/tokens.test.ts`. Parse `tokens.css?raw` into blocks (`:root`, `.dark`, `[data-density='comfortable']`) and a resolver that follows `var(--x)` chains to a final value.
  - `accent is blue in both themes`: resolve `--dnt-accent` → `#2563d9` (light), `#6b9bf2` (dark).
  - `every semantic token is defined for light and dark`: every `--dnt-` name defined in `:root` that is not a primitive or component token is also defined in `.dark`.
  - `secondary text meets 4.5:1 on chrome`: WCAG contrast of `fg-muted` and `fg-subtle` against `bg-chrome` ≥ 4.5 in both themes.
  - `comfortable density raises control heights`: `--dnt-ctl-h-md` is `28px` on `:root` and `30px` under `[data-density='comfortable']`.
  - `legacy names still resolve`: `--studio-accent`, `--accent`, `--sel`… resolve to the new values (`--studio-accent` → `#2563d9`).
- [ ] **Step 2: Run** `pnpm --dir ui exec vitest run src/theme/tokens.test.ts` — expected FAIL (no `--dnt-` tokens).
- [ ] **Step 3: Rewrite `tokens.css`** with the values above.
- [ ] **Step 4: Run** the new test plus `src/theme` (cssVars, tokenCompliance) — expected PASS.
- [ ] **Step 5: Commit** `feat(ui): three-layer --dnt- design tokens with legacy aliases`.

### Task 2: Base interaction rules

**Files:**
- Modify: `ui/src/theme.css` (app aliases at lines 6–50, focus rules ~84–89 and ~229–278, `.tool:disabled` ~143, `.context-menu button:disabled` ~483, `.tree-row.selected`)
- Modify: `ui/src/dialogs/dialogs.css` (`.btn:disabled` ~105), `ui/src/tabs/query/query.css` (`.qt-btn:disabled` ~34, `.qt-menu-item:disabled` ~332)
- Modify: `ui/src/theme/tokenCompliance.test.ts`

**Interfaces:**
- Consumes: Task 1 tokens.
- Produces: one focus rule set; selection classes unchanged (`.tree-row.selected`), now muted unless the tree has focus.

- [ ] **Step 1: Add failing tests** to `tokenCompliance.test.ts`:
  - `disabled is never opacity`: no rule whose selector contains `:disabled` declares `opacity`.
  - `focus is at most 1px and never a ring`: no `outline` wider than `1px` anywhere; no `box-shadow` other than `none` in a rule whose selector contains `:focus`.
  - `focus is visible on rows and menu items`: `theme.css` contains a rule for `[role='menuitem']:focus-visible` and `[role='option']:focus-visible` with `outline: 1px solid var(--dnt-focus)`; no `outline: none` for those selectors.
- [ ] **Step 2: Run** `pnpm --dir ui exec vitest run src/theme` — expected FAIL on the opacity rules and the menu-item `outline: none`.
- [ ] **Step 3: Implement:**
  - App aliases in `theme.css` point at `--dnt-*` (`--panel→bg-app`, `--sidebar/--tabstrip→bg-chrome`, `--muted/--icon→fg-muted`, `--sel→bg-selected`, `--hover→bg-hover`, `--danger-fg/--production-fg→status-danger-fg`, `--danger-fill/--production-bg→status-danger-bg`, `--danger-border→status-danger-border`, `--production→env-prod`, `--warning-bg/--warning-fg→status-warning-bg/-fg`); delete their hex values and the `.dark` overrides that only restated them (keep `--titlebar-close-*`).
  - One focus block: `:focus-visible { outline: 1px solid var(--dnt-focus); outline-offset: -1px }`; `input, select, textarea` (and `[contenteditable]`) `:focus-visible { outline: none; border-color: var(--dnt-focus) }`; Monaco (`.monaco-editor :focus-visible`) keeps `outline: none`; tree keeps its own focus on the selected row (`outline: 1px solid var(--dnt-focus)`). Remove the old duplicated blocks.
  - Disabled rules use `color: var(--dnt-fg-disabled)` (and `background`/`border-color` from `bg-sunken`/`border-subtle` for `.btn`); drop `opacity`.
  - Selection: `.tree-row.selected { background: var(--dnt-bg-selected-muted) }`, `.tree[role='tree']:focus-within .tree-row.selected { background: var(--dnt-bg-selected) }`; same pairing for `.fav-tree` and `.palette-row[aria-selected='true']` if they use `--sel`.
- [ ] **Step 4: Run** `pnpm --dir ui exec vitest run` — expected PASS (fix any component test that asserted `opacity`).
- [ ] **Step 5: Commit** `feat(ui): one 1px focus rule, disabled without opacity, muted selection`.

### Task 3: Grid and Monaco read the new tokens

**Files:**
- Modify: `ui/src/tabs/query/ResultGrid.tsx:134-179` (`buildTheme`)
- Modify: `ui/src/monaco/dntTheme.ts:33-51`
- Test: `ui/src/tabs/query/ResultGrid.test.tsx` (or a new `gridTheme.test.ts` if `buildTheme` is exported for it)

**Interfaces:**
- Consumes: Task 1 semantic tokens.
- Produces: `export function buildTheme()` (exported for the test; signature unchanged).

- [ ] **Step 1: Failing test** `grid theme follows the dark class`: define `--dnt-accent`, `--dnt-bg-app`, `--dnt-fg` on `document.documentElement` for light, call `buildTheme()`, assert `accentColor`/`bgCell`/`textDark`; set dark values + `.dark`, call again, assert the dark values.
- [ ] **Step 2: Run** it — expected FAIL (reads `--accent`/`--panel`).
- [ ] **Step 3: Implement** `buildTheme` reading `--dnt-accent`, `--dnt-bg-selected` (accentLight, header focus/hover), `--dnt-bg-app` (bgCell), `--dnt-bg-sunken` (bgHeader), `--dnt-fg`, `--dnt-fg-muted`, `--dnt-border-subtle` (borderColor), `--dnt-status-warning-bg/-fg`, `--dnt-status-danger-bg/-fg`; fallbacks = Task 1 light values. `dntTheme.ts`: `bg-app`, `fg`, `fg-subtle` (line numbers), `fg-muted` (active line number), `bg-selected`, `bg-selected-muted` (inactive selection), `bg-sunken` (line highlight), `bg-raised` (widgets), `border` (widget borders), `border-strong` (scrollbar).
- [ ] **Step 4: Run** `pnpm --dir ui exec vitest run src/tabs/query src/monaco` — PASS.
- [ ] **Step 5: Commit** `feat(ui): grid and editor themes read --dnt- tokens`.

### Task 4: Density setting

**Files:**
- Modify: `ui/src/theme/ThemeProvider.tsx`, `ui/src/theme/ThemeProvider.test.tsx`
- Modify: `ui/src/dialogs/SettingsDialog.tsx` (after the Appearance field), `ui/src/dialogs/SettingsDialog.test.tsx`

**Interfaces:**
- Produces: `export type Density = 'compact' | 'comfortable'`; `useTheme()` adds `density: Density` and `setDensity(d: Density): void`. Stored in `localStorage['density']`; applied as `document.documentElement.dataset.density` (attribute removed for compact).

- [ ] **Step 1: Failing tests** in `ThemeProvider.test.tsx`: `density defaults to compact` (no attribute); `setDensity('comfortable') sets data-density and persists`; `restores stored density`; `ignores an unknown stored value`. In `SettingsDialog.test.tsx`: `Density radio group switches to Comfortable` (role radiogroup name "Density", radios "Compact" / "Comfortable").
- [ ] **Step 2: Run** — FAIL.
- [ ] **Step 3: Implement** following the existing theme pattern (`readDensity` with try/catch, layout effect sets the attribute); SettingsDialog adds a "Density" field identical in markup to Appearance (`theme-options` / `theme-option`).
- [ ] **Step 4: Run** `pnpm --dir ui exec vitest run src/theme src/dialogs` — PASS.
- [ ] **Step 5: Commit** `feat(ui): density setting (compact default)`.

### Task 5: Phase 1 verification

- [ ] `pnpm --dir ui lint` → clean.
- [ ] `pnpm --dir ui test` → all pass.
- [ ] `pnpm e2e:web` → all pass.
- [ ] Screenshots (temporary e2e-web spec, deleted afterwards) of empty, tree, data, query, connection dialog in light and dark; check: blue selection / primary, focus 1 px, no opacity-disabled, dark mode complete.

---

## Phases 2–6 (outline; each gets its own detailed plan before it starts)

- **Phase 2 — Shell:** TitleBar search button (opens palette, objects mode); delete `Toolbar.tsx` and move `new-connection` (sidebar header) and `new-query` (tab strip `+`) testids; placeholder features as disabled "Coming soon" menu items; Sidebar segmented Objects / Favorites (FavoritesPanel becomes a full-height view, real icon for Manage snippets); distinct folder icons; flat tabs; status bar env segment and per-tab-type right side. Update Toolbar/TitleBar/TabBar/StatusBar/QueryTab tests and e2e helpers if any selector changes.
- **Phase 3 — Editor groups:** extend `store/tabs.ts` with `groups`, `focusedGroupId`, `layout`, ratios; derived `activeId`; workspace renders groups with splitters; drag-to-split, tab menu items, shortcuts; persistence with the workspace; narrow-group toolbar collapse.
- **Phase 4 — Object tab:** single-row header with read-only badge and segmented sub-tabs; remove duplicate Query/Refresh; Data toolbar regrouped with `⋯` menu; grid row/header heights from density tokens; no lines below last row.
- **Phase 5 — Query tab:** Run split (Run on PROD variant, Stop in place), context pill picker, Limit compact, transaction chip, items moved into the Run menu, no editor placeholder, results collapsed until first run.
- **Phase 6 — Overlays:** modal frame, connection dialog layout, palette sections / mode chip / highlights / Recent, toasts, production confirm; remove legacy token names and add the "no `--studio-*` outside tokens.css" test.
