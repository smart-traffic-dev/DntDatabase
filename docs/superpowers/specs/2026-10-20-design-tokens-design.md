# Design tokens from dnt-note

Date: 2026-10-20
Status: approved (user chose option A + dark mode + neutral semantics + system fonts)

## Goal

Make DntDatabase use the design tokens of `dungnt1312/dnt-note` (commit `170211f`,
`src/theme/tokens.css`) exactly: same token names, values, light/dark pairs, type scale,
radii, shadow and font stacks. Layout and component behaviour stay as they are; this is a
token and styling pass, not a redesign.

## Source

`dnt-note/src/theme/tokens.css` defines:

- `--studio-*` colours for light (`:root`) and dark (`.dark`), including accent (`#0d0d0d`
  light / `#f3f3f3` dark), danger, hairline, hover, selected, control, subtle, popover,
  scrollbar and shadow.
- Tailwind `@theme inline` aliases (`--color-*`, `--shadow-popover`) and `@theme`
  typography (`--font-sans`, `--font-mono`, `--text-*`, `--text-*--line-height`,
  `--radius-*`).
- Legacy aliases (`--bg`, `--text`, `--border`, `--accent`, `--danger`, …).
- Base rules: body, focus ring, placeholder, selection, scrollbar, reduced motion.

## Adaptation

DntDatabase does not use Tailwind. The token file is copied into
`ui/src/theme/tokens.css` with the **same variable names and values**. The only change is
declaration syntax: `@theme inline` / `@theme` blocks become `:root`, because browsers
ignore unknown at-rules. No Tailwind dependency is added.

`ui/src/theme.css` imports the token file first and defines app compatibility aliases:

| App alias | Token |
| --- | --- |
| `--panel` | `--studio-bg` |
| `--sidebar` | `--studio-sidebar` |
| `--muted` | `--studio-fg-secondary` |
| `--sel` | `--studio-selected` |
| `--tabstrip` | `--studio-sidebar` |
| `--icon` | `--studio-fg-secondary` |
| `--production` | `--studio-danger` |
| `--production-bg` | `--studio-danger-fill` |
| `--staging`, `--local` | `--studio-fg-secondary` |

All remaining raw colours in `theme.css`, `dialogs.css`, `query.css` and `objectTab.css`
are replaced by `--studio-*` tokens. The existing `--bg`, `--text`, `--border`,
`--accent`, `--danger` names resolve through the copied legacy aliases.

## Semantics without a token

The dnt-note palette is monochrome and has no per-environment or per-engine colours.
Per the approved option 3a:

- Production uses `--studio-danger` / `--studio-danger-fill`.
- Staging and local use neutral `--studio-fg-secondary`; a letter badge (`P` / `S` / `L`)
  carries the meaning so it is not colour-only. Production keeps its `PRODUCTION` badge in
  the status bar.
- Engine chips (`MY`, `PG`, `MA`) become neutral (`--studio-subtle` background,
  `--studio-fg-secondary` text, `--studio-hairline` border).
- Success feedback has no token: use `--studio-subtle` + `--studio-fg`; errors keep
  `--studio-danger*`.

## Typography and shape

- Base body: `--text-body` (15px/24px). Dense app chrome (tree, tabs, toolbar, dialogs)
  uses `--text-callout` (13px/18px); metadata uses `--text-footnote` (12px) or
  `--text-caption` (11px). The 10px sizes are removed because the scale forbids them.
- Dialog titles use `--text-headline` (17px/24px).
- `--font-sans` and `--font-mono` are the dnt-note system stacks; the `@fontsource`
  IBM Plex / JetBrains imports and dependencies are removed.
- Radii: controls and chips `--radius-sm` (8px); dialogs, menus, toasts and cards
  `--radius-md` (12px); tabs `--radius-sm` on the top corners.
- Popover shadow: `--shadow-popover` (the single `--studio-shadow` value).
- Focus: `2px solid var(--studio-fg-secondary)` with `2px` offset; inputs and menu rows
  show focus through their border, per the source's base rules.
- Scrollbars: thin, `--studio-scrollbar`.
- `user-select: none` from the source body is **not** copied: a database tool must allow
  selecting DDL, object names and error text.

## Dark mode

The `.dark` token block is copied. A `ThemeProvider` (light / dark / system) toggles the
`dark` class on `<html>`, persists the choice in `localStorage` and reacts to
`prefers-color-scheme` when set to system. Settings gets a three-option appearance
selector. Monaco switches between `vs` and `vs-dark`.

## Files

- New: `ui/src/theme/tokens.css`, `ui/src/theme/ThemeProvider.tsx`.
- Updated: `ui/src/theme.css`, `ui/src/dialogs/dialogs.css`, `ui/src/tabs/query/query.css`,
  `ui/src/tabs/objectTab.css`, `ui/src/main.tsx`, `ui/src/App.tsx`,
  `ui/src/dialogs/SettingsDialog.tsx`, `ui/src/tabs/query/SqlEditor.tsx`,
  `ui/src/components/ConnectionTree.tsx`, `ui/src/components/TabBar.tsx`,
  `ui/package.json` (+ lockfile).
- The `.tab` class collision between app tabs and dialog tabs is fixed by renaming the
  dialog tab class to `.dlg-tab`, because both definitions currently fight over border,
  radius and padding.

## Out of scope

- Component redesign (capsule buttons, icon-button sizes, sidebar width).
- Layout, interaction, query behaviour and keyboard shortcuts.
- New semantic tokens beyond the compatibility aliases above.

## Verification

- `pnpm --dir ui lint`, `pnpm --dir ui test`, `pnpm --dir ui build`.
- Visual check of the shell, connection dialog, query tab and settings in light and dark at
  1440px, using the Vite dev server (no Tauri backend).
- No remaining raw hex/rgba values in `ui/src/**/*.css` except the modal backdrop, which
  has no token in the source and is defined per theme.
