# UX/UI redesign: tokens, shell, object tab, query tab, overlays

- Date: 2026-10-21
- Status: proposed for review, not implemented.
- Design source: canvas "DntDatabase UX redesign" (https://claude.ai/artifact/3cyv2WneP9K6Nftmg9E8Qa), boards 01–08.
  Boards 09–19 (P4–P7 features) are implemented with those features, not here.
- Supersedes: `2026-10-20-design-tokens-design.md` ("use dnt-note tokens exactly, no redesign").
- Previous work-in-progress (connection dialog / query toolbar restyle) is stashed as
  `stash@{0}: wip before UX redesign …` and is not part of this work.

## 1. Decisions already made

| Topic | Decision |
|---|---|
| Accent | One blue accent, `#2563D9` light / `#6B9BF2` dark, for selection, focus, primary buttons, links, checkboxes. Chrome stays neutral grey. |
| Big toolbar | Removed. Menu + search live in the title bar; New query on the tab strip; New connection in the sidebar. Unbuilt features appear in menus as disabled "Coming soon" items. |
| Density | Compact by default; Comfortable is a setting. |
| Tabs | Flat. Active tab = surface fill joined to the content + medium weight. No rounded top, no coloured top bar. Production is shown by a PROD badge. |
| Focus | No thick outline, ring or glow. Bordered controls: the 1 px border turns accent. Rows, menu items, grid cells, tabs: 1 px inset accent outline. |
| Copy | All UI text is English. |

## 2. Design tokens (phase 1)

### 2.1 Layers

`ui/src/theme/tokens.css` is rewritten with three layers, prefix `--dnt-`:

1. **Primitive**: `--dnt-gray-{0,25,50,100,200,300,400,500,600,700,800,850,900,950}`,
   `--dnt-blue-{50,100,400,500,600,700}`, `--dnt-red-{50,200,500,700}`, `--dnt-amber-{50,200,500,700}`,
   `--dnt-green-{50,200,600}`. Values as on canvas board 02. Only the token file reads this layer.
2. **Semantic** (light on `:root`, dark on `.dark`):
   - surfaces `bg-app`, `bg-chrome`, `bg-sunken`, `bg-raised`, `bg-hover`, `bg-active`, `bg-selected`,
     `bg-selected-muted`;
   - borders `border-subtle`, `border`, `border-strong`;
   - text `fg`, `fg-muted`, `fg-subtle`, `fg-disabled`;
   - `accent`, `accent-hover`, `on-accent`, `accent-subtle`, `focus`;
   - `status-{danger,warning,success,info}-{fg,bg,border,solid}`;
   - `env-prod`, `env-staging`, `env-local` (aliases onto status tokens / neutral);
   - `shadow-1/2/3`, `scrim`.
3. **Component** (density-dependent, on `:root` and `[data-density='comfortable']`):
   `ctl-h-{xs,sm,md,lg}`, `row-h-tree`, `row-h-grid`, `row-h-menu`, `titlebar-h`, `tab-h`, `statusbar-h`,
   `icon-size`; plus density-independent `space-{0_5,1,1_5,2,3,4,6,8}`, `radius-{xs,sm,md,lg,full}`,
   `text-{xs,sm,base,md,lg,xl,2xl}` (+ `--lh`), `dur-{1,2,3}`, `ease-std`, `ease-exit`,
   `z-{sticky,dropdown,overlay,modal,toast,tooltip}`.

Values: board 02. Contrast: `fg-muted` and `fg-subtle` ≥ 4.5:1 on `bg-chrome` in both themes.

### 2.2 Migration without breaking

- Phase 1 keeps every existing name working: `--studio-*`, `--bg`, `--text`, `--panel`, `--muted`, `--sel`, … are
  redefined as aliases of the new semantic tokens (mapping table on board 02). Result: the whole app re-tints
  (blue selection/primary, new greys) with no component change.
- Chat-only tokens (`composer*`, `bubble`, `send-disabled*`, `mention`, `radius-lg 22`, `radius-xl 26`,
  `text-body 15`) are removed once no file references them.
- CSS files are then moved to `--dnt-*` names file by file in phases 2–5. A file touched by a phase must end
  with no legacy names.
- `theme/tokenCompliance.test.ts` gains: no `--studio-*` outside the token file (after phase 5), no
  `opacity` on `:disabled`, no `outline` wider than 1 px, no `box-shadow` used as a focus ring.
  `theme/cssVars.test.ts` keeps checking every `var()` is defined.
- `ResultGrid.buildTheme()` and `monaco/dntTheme.ts` read the new semantic tokens (fallback hex values
  updated to the new palette).

### 2.3 Base rules

- `:focus-visible { outline: 1px solid var(--dnt-focus); outline-offset: -1px }` for everything; text inputs,
  selects and textareas use `border-color: var(--dnt-focus)` and `outline: none`. The current double definition
  (tokens.css then theme.css) is replaced by this one.
- Selection: `bg-selected` while the owning pane has focus (`:focus-within`), `bg-selected-muted` otherwise. Applies
  to tree rows, palette rows, favorites rows, menus. The grid uses the same two colours through its theme.
- Disabled: `color: var(--dnt-fg-disabled)`, never opacity. Disabled controls keep their reason tooltip.
- Density: Settings → General → Density (Compact / Comfortable) sets `data-density` on `<html>`; stored with the
  other settings.

## 3. Shell (phase 2)

Boards 04 and 05.

- **Title bar (36 px):** logo, menu bar (unchanged behaviour), centred search button "Search tables, views,
  commands…" with `Ctrl P` keycap that opens the command palette in objects mode, window buttons. The window
  title (`document.title`) still carries "{tab} - DntDatabase"; the in-bar title text is dropped.
- **Toolbar removed.** Its actions move:
  - New connection → sidebar header `+` (testid `new-connection`), File menu (already there).
  - Connection menu items (transfer, Settings…) → File / Tools menus (keep existing ids).
  - New query → tab strip `+` button after the last tab (testid `new-query`), File menu, `Ctrl+T`.
  - The seven placeholders → disabled menu items labelled "Coming soon" in the menus that will own them.
- **Sidebar:** header row = segmented `Objects | Favorites` + `+` New connection + `⋯` (show system schemas,
  refresh all, collapse all). The filter box sits under it. Favorites becomes a full-height view instead of
  the fixed bottom panel; its "Manage snippets" button gets a real icon. Tree rows use `row-h-tree`; folder
  kinds get distinct icons (table, view, function, procedure, trigger, event); the engine/version text moves to
  the connection row tooltip; counts stay as right-aligned captions.
- **Tab strip (34 px):** flat tabs per §1; close button visible on the active tab and on hover; dirty dot, tx dot,
  pin and PROD/STG/LCL badge unchanged in meaning.
- **Status bar (24 px):** first segment shows the environment with its colour (PRODUCTION / STAGING / LOCAL);
  then connection, database. Right side is per tab type: query tabs show cursor + encoding, data tabs show
  selection summary and row count. The `.env-production` red wash of the whole bar is replaced by the
  segment.

## 4. Object tab and Data view (phase 3)

Board 04.

- **Header (40 px), one row:** object name, `connection › database` path, Read-only badge (lock icon, tooltip
  "This connection is read-only. Change it in Edit connection."), then the sub-tabs as a segmented control on the
  right. The separate "Read-only connection." line is removed.
- **No duplicate actions:** header "Query" and "Refresh" go away. Data sub-tab: Refresh lives in the data
  toolbar and reloads metadata too. DDL / Structure / Info: one Refresh icon button at the right end of their
  panel toolbars.
- **Data toolbar (40 px):** left = Filter (existing FilterBar toggle, F3) and the sort chip(s);
  right = pagination cluster `1–100 / ~3,412` with ‹ › icon buttons, a divider, Refresh, Open in query, Export
  (the only secondary-styled button), `⋯` menu holding Page size and Count rows. Behaviour of every control is
  unchanged.
- **Grid:** row height `row-h-grid` (24 compact / 30 comfortable), header 36 px with name on line 1 and type on line
  2 (caption, never wrapping), numbers right-aligned with tabular figures, NULL in italic `fg-subtle`, no cell
  lines painted below the last row.

## 5. Query tab (phase 4)

Board 05.

- **Toolbar (40 px):** Run split button (primary). On a Production connection it renders as danger-outline with
  the label "Run on PROD". While running, the same slot shows Stop (danger). Then Explain (split, existing menu),
  Format, divider, **context pill** `[ENV] connection › database` (one control opening a picker with the two
  existing selects), Limit (compact select), and on the right the transaction area: Auto/Manual segment, and
  when a transaction is open a warning chip "Transaction · mm:ss" + Rollback + Commit.
  "Run all", "Run selection", "Continue on error" and Add to favorites move into the Run split menu.
- **Editor:** no placeholder text inside the editor.
- **Results panel:** collapsed to its 32 px tab bar until the first run of the tab; the empty state there shows
  the run shortcuts. After the first run it opens at the saved ratio.

## 6. Dialogs and overlays (phase 5)

Boards 06 and 08.

- **Modal frame:** header (title + close), body, footer; radius `lg`, `shadow-3`, `scrim`. Primary action right;
  Enter triggers the safe default in destructive dialogs.
- **Connection dialog:** vertical section nav (General, SSH tunnel, SSL / TLS, Advanced) with a state hint on each;
  Engine is the first field (select with engine badge, default port note); Environment as a radio chip group
  None / Local / Staging / Production with a one-line explanation of what Production changes; Test button and its
  result (idle / testing + Cancel / connected / failed + Details) in the footer's left side.
- **Command palette:** mode chip (Commands / Objects) instead of a typed `>` (typing `>` still switches); rows
  grouped under section headers (Recent first, then menu name) without the "File:" prefix; matched characters
  highlighted in `accent`; shortcuts as keycaps; disabled "Coming soon" items listed last.
- **Toasts:** bottom-right stack of at most 3, `bg-raised` + `shadow-2`; success/info auto-hide after 4 s (paused on
  hover/focus); errors stay until dismissed and offer Copy details; progress toasts show a bar.
- **Production confirm:** existing RunConfirmDialog restyled to board 08 (PRODUCTION badge, SQL block, typed
  name, danger solid primary).

## 7. Behaviour rules carried into code

From board 03; each is checked by a test where a test is practical:

1. Spinner appears after 150 ms and stays at least 400 ms once shown.
2. Every icon-only button has `aria-label` and a tooltip with its shortcut; tooltips open after 500 ms.
3. Disabled controls state their reason (existing pattern, extended to every disabled toolbar control).
4. Esc closes the top-most layer and returns focus to its opener (existing in ActionMenu / Modal; verify for the
   palette and the context pill).
5. Splitters: 8 px hit area, 1 px line, accent on hover, double-click resets.

## 8. Testing and verification

- Unit tests that assert removed structure are updated, not deleted: Toolbar tests move to the new owners of
  `new-connection` / `new-query` (sidebar header, tab strip); TitleBar tests drop the in-bar title text; TabBar,
  StatusBar, EnvTag tests follow the new classes; QueryTab split test follows the collapsed-results rule.
- e2e (WebdriverIO) and e2e-web keep working because the `new-connection` and `new-query` testids move with their
  buttons.
- Each phase ends with `pnpm --dir ui lint`, `pnpm --dir ui test`, `pnpm e2e:web`, and e2e-web screenshots of the
  empty, tree, data, query and dialog states in light and dark, compared against the canvas.

## 9. Phases

Each phase is its own commit series and leaves the app shippable.

1. Tokens + base rules + density setting (app re-tints; layout unchanged).
2. Shell: title bar search, toolbar removal, sidebar views, flat tabs, status bar.
3. Object tab header + Data toolbar + grid metrics.
4. Query toolbar, context pill, results collapse.
5. Dialog frame, connection dialog, palette, toasts, confirm dialog; remove legacy token names.

Out of scope: P4–P7 feature screens, AI, new settings beyond Density.
