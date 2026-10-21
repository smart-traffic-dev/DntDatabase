import { create } from 'zustand';
import type { ObjectRef, Scope } from '../bindings';
import {
  clampRatio,
  isLayoutNode,
  leaves,
  presetOf,
  presetTree,
  removeLeaf,
  setRatio,
  splitLeaf,
  type LayoutNode,
  type LayoutPreset,
  type Side,
} from './layoutTree';
import type { QueryDoc } from './queryTabs';

export interface ObjectTabRef {
  id: string;
  kind: 'object';
  /** A DDL outcome is unknown: hide cached data until the user verifies the object. */
  needsVerification?: boolean;
  resourceRevision?: number;
  connectionId: string;
  objectRef: ObjectRef;
  title: string;
  /** Pinned tabs sort first and are skipped by "Close others" / "Close tabs to the right". */
  pinned?: boolean;
  /** A preview tab (italic title): the next preview opened in its group replaces it. */
  preview?: boolean;
}

/** A query tab; its document lives in `useQueryDocs` under the same id. */
export interface QueryTabRef {
  id: string;
  kind: 'query';
  connectionId: string | null;
  title: string;
  pinned?: boolean;
  preview?: boolean;
}

/** A table designer tab; `table` is null while designing a new table. The draft lives in `useTableDesigns`. */
export interface DesignTabRef {
  id: string;
  kind: 'design';
  connectionId: string;
  scope: Scope;
  table: string | null;
  title: string;
  pinned?: boolean;
  preview?: boolean;
}

export type Tab = ObjectTabRef | QueryTabRef | DesignTabRef;

/** What "Reopen closed tab" brings back: an object tab by reference, a query tab with its document. */
export type ClosedTab = { kind: 'object'; tab: ObjectTabRef } | { kind: 'query'; doc: QueryDoc };

/** How many closed tabs are remembered. */
export const CLOSED_STACK_MAX = 10;

/** The shape of the editor area: one of the presets, or any other split tree (`custom`). */
export type EditorLayout = LayoutPreset | 'custom';

export type { LayoutNode, Side } from './layoutTree';

/** A pane of the workspace with its own tab strip. Its tabs are the tabs whose `groupOf` is its id. */
export interface EditorGroup {
  id: string;
  activeId?: string;
}

/** What is saved across restarts: the split tree and which tabs sat in which group. */
export interface Arrangement {
  v: 2;
  tree: LayoutNode;
  groups: { id: string; tabIds: string[]; activeId?: string }[];
  focused: string;
}

/** The first saved shape (fixed presets with two shared ratios); still read on restore. */
export interface ArrangementV1 {
  layout: LayoutPreset;
  ratios: { x: number; y: number };
  groups: { tabIds: string[]; activeId?: string }[];
  focused: number;
}

export const MAX_GROUPS = 4;

interface TabsState {
  /** Every open tab, in strip order; each sits in exactly one group (`groupOf`). */
  tabs: Tab[];
  /** The active tab of the focused group (what Run, the shortcuts, the status bar and the title act on). */
  activeId?: string;
  /** In reading order: the leaves of `tree`. */
  groups: EditorGroup[];
  focusedGroupId: string;
  /** Tab id → group id. */
  groupOf: Record<string, string>;
  /** The editor area as a split tree; its leaves are the groups. */
  tree: LayoutNode;
  /** The preset the tree matches (`custom` otherwise): what the layout menus tick. */
  layout: EditorLayout;
  /** A group shown alone over the whole editor area (the tree is kept for the restore). */
  maximizedGroupId?: string;
  focusGroup(groupId: string): void;
  /**
   * Splits `groupId` on `side`: a new group there, the rest of the tree untouched. With `tabId` the
   * tab moves into the new group; without, the new group starts empty and focused.
   */
  splitGroup(groupId: string, side: Side, tabId?: string): void;
  /** Moves `tabId` into a new group on `side` of its own (an empty one when it is its group's only tab). */
  splitTab(tabId: string, side: Side): void;
  /** Moves the focused group's active tab one place left (`-1`) / right (`1`) in its strip. */
  shiftTab(step: 1 | -1): void;
  /** A new group can still be added. */
  canSplit(): boolean;
  /** Moves a tab into `groupId` (at `index` among that group's tabs, else last); it becomes active there. */
  moveTab(tabId: string, groupId: string, index?: number): void;
  /** Moves the active tab to the next (`1`) / previous (`-1`) group. */
  moveToNeighbour(step: 1 | -1): void;
  /** Focuses the next / previous group, wrapping around. */
  focusNeighbour(step: 1 | -1): void;
  setLayout(layout: LayoutPreset): void;
  /** Removes a group that holds no tab (never the last group). */
  dropGroup(groupId: string): void;
  /** The share of a split node's first side (clamped). */
  setSplitRatio(splitId: string, ratio: number): void;
  /** Maximizes the group (default: the focused one), or restores when it already is. */
  toggleMaximize(groupId?: string): void;
  arrangement(): Arrangement;
  /** Puts the open tabs back into a saved arrangement (tabs it does not know go to the first group). */
  applyArrangement(saved: Arrangement | ArrangementV1): void;
  /** Last closed tabs, most recent last (at most `CLOSED_STACK_MAX`). */
  closed: ClosedTab[];
  /** Remembers a closed tab (called by whoever closes a query tab; object tabs record themselves). */
  pushClosed(entry: ClosedTab): void;
  /** Takes the most recently closed tab off the stack. */
  popClosed(): ClosedTab | undefined;
  /** Pins or unpins a tab: pinned tabs stay at the left (and are never previews). */
  setPinned(id: string, pinned: boolean): void;
  /** Opens a tab in the focused group; a tab that is already open is activated in its own group. */
  open(tab: Tab): void;
  /**
   * Opens a tab as the focused group's preview: it replaces that group's current preview tab.
   * A tab already open is only activated (and keeps what it is).
   */
  openPreview(tab: Tab): void;
  /** Makes a preview tab a normal one (double-click, an edit, sorting, filtering, pinning). */
  keepTab(id: string): void;
  /** Activates a tab and focuses its group. */
  activate(id: string): void;
  /** Rebinds a query tab to another connection (or none). */
  bind(id: string, connectionId: string | null): void;
  rename(id: string, title: string): void;
  /** DDL changes object identities without moving tabs between groups. */
  replaceObjectTabs(replacements: Record<string, ObjectTabRef | null>): void;
  /**
   * A design tab now designs `table` (it was created or renamed): the tab keeps its place, group and
   * focus but takes the id of the new table. Returns the new id, or null when the tab is gone or
   * another tab already has that id (nothing is changed then).
   */
  moveDesignTab(id: string, table: string): string | null;
  close(id: string): void;
  /** Closes every object and design tab of a connection (it was deleted or replaced); query tabs keep their binding. */
  closeConnection(connectionId: string): void;
  reset(): void;
}

/** One tab per object of a connection. */
export function objectTabId(connectionId: string, o: ObjectRef): string {
  return [connectionId, o.database, o.schema ?? '', o.kind, o.name].map(encodeURIComponent).join('/');
}

/** One designer tab per table of a connection (one "new table" tab per scope: `table` null). */
export function designTabId(connectionId: string, scope: Scope, table: string | null): string {
  return ['design', connectionId, scope.database, scope.schema ?? '', table ?? '\u0000new'].map(encodeURIComponent).join('/');
}

/** The tabs of one group, in strip order. */
export const groupTabs = (s: Pick<TabsState, 'tabs' | 'groupOf'>, groupId: string): Tab[] =>
  s.tabs.filter((t) => s.groupOf[t.id] === groupId);

let groupSeq = 0;
const newGroupId = () => `g${++groupSeq}`;
let splitSeq = 0;
const newSplitId = () => `s${++splitSeq}`;

type Core = Pick<TabsState, 'tabs' | 'groups' | 'focusedGroupId' | 'groupOf' | 'tree' | 'layout' | 'activeId' | 'maximizedGroupId'>;

/**
 * Restores the invariants after a change: the groups are the tree's leaves in reading order,
 * every tab sits in one of them, a focused group, each group's active tab one of its own, the
 * layout name matching the tree, `activeId` the focused group's active tab, and a maximized
 * group that still exists.
 */
function settle(s: Core): Core {
  const tree: LayoutNode = s.tree ?? { kind: 'group', id: newGroupId() };
  const byId = new Map(s.groups.map((g) => [g.id, g]));
  const groups = leaves(tree).map((id) => byId.get(id) ?? { id });
  const known = new Set(groups.map((g) => g.id));
  const focusedGroupId = known.has(s.focusedGroupId) ? s.focusedGroupId : groups[0]!.id;
  const groupOf: Record<string, string> = {};
  for (const t of s.tabs) {
    const g = s.groupOf[t.id];
    groupOf[t.id] = g && known.has(g) ? g : focusedGroupId;
  }
  const fixed = groups.map((g) => {
    const members = s.tabs.filter((t) => groupOf[t.id] === g.id);
    if (g.activeId && members.some((t) => t.id === g.activeId)) return g;
    return { ...g, activeId: members.at(-1)?.id };
  });
  const focused = fixed.find((g) => g.id === focusedGroupId);
  return {
    tabs: s.tabs,
    groups: fixed,
    focusedGroupId,
    groupOf,
    tree,
    layout: presetOf(tree),
    activeId: focused?.activeId,
    maximizedGroupId: s.maximizedGroupId && known.has(s.maximizedGroupId) && fixed.length > 1 ? s.maximizedGroupId : undefined,
  };
}

/** Removes `groupId` when it holds no tab and is not the last group; its neighbour takes the focus. */
function dropIfEmpty(s: Core, groupId: string): Core {
  if (s.groups.length < 2 || s.tabs.some((t) => s.groupOf[t.id] === groupId)) return s;
  const at = s.groups.findIndex((g) => g.id === groupId);
  if (at < 0) return s;
  const groups = s.groups.filter((g) => g.id !== groupId);
  const focusedGroupId = s.focusedGroupId === groupId ? (groups[Math.max(0, at - 1)] ?? groups[0])!.id : s.focusedGroupId;
  return { ...s, groups, focusedGroupId, tree: removeLeaf(s.tree, groupId) ?? s.tree };
}

const setGroupActive = (groups: EditorGroup[], groupId: string, activeId: string | undefined) =>
  groups.map((g) => (g.id === groupId ? { ...g, activeId } : g));

/** Moves a tab into a group (strip position `index` among its tabs, pinned block kept first). */
function moveInto(s: Core, tabId: string, groupId: string, index?: number): Core | null {
  const from = s.groupOf[tabId];
  const tab = s.tabs.find((t) => t.id === tabId);
  if (!from || !tab || !s.groups.some((g) => g.id === groupId)) return null;
  const rest = s.tabs.filter((t) => t.id !== tabId);
  const targets = rest.filter((t) => s.groupOf[t.id] === groupId);
  // Pinned tabs stay first: a pinned tab lands inside the pinned block, an unpinned one after it.
  const pinnedCount = targets.filter((t) => t.pinned).length;
  const wanted = index ?? targets.length;
  const slot = tab.pinned ? Math.min(wanted, pinnedCount) : Math.max(wanted, pinnedCount);
  const anchor = targets[slot];
  const at = anchor ? rest.indexOf(anchor) : targets.length ? rest.indexOf(targets.at(-1)!) + 1 : rest.length;
  const tabs = [...rest.slice(0, at), tab, ...rest.slice(at)];
  // The source group's active tab moves on to a neighbour.
  const left = groupTabs(s, from);
  const i = left.findIndex((t) => t.id === tabId);
  const neighbour = (left[i + 1] ?? left[i - 1])?.id;
  let groups = setGroupActive(s.groups, groupId, tabId);
  if (from !== groupId && s.groups.find((g) => g.id === from)?.activeId === tabId) groups = setGroupActive(groups, from, neighbour);
  // The target takes the focus: a maximized view of another group would hide it, so it ends.
  const maximizedGroupId = s.maximizedGroupId && s.maximizedGroupId !== groupId ? undefined : s.maximizedGroupId;
  const moved: Core = { ...s, tabs, groups, groupOf: { ...s.groupOf, [tabId]: groupId }, focusedGroupId: groupId, maximizedGroupId };
  return dropIfEmpty(moved, from);
}

/**
 * A new tab's place in the strip: right after its group's active tab (after the pinned block when
 * that one is pinned), as VS Code opens editors; at the end when the group has no active tab.
 */
function insertNew(s: Core, tab: Tab, groupId: string): Tab[] {
  const members = groupTabs(s, groupId);
  const active = members.find((t) => t.id === s.groups.find((g) => g.id === groupId)?.activeId);
  const anchor = active?.pinned && !tab.pinned ? members.filter((t) => t.pinned).at(-1) : active;
  if (!anchor) return [...s.tabs, tab];
  const at = s.tabs.indexOf(anchor) + 1;
  return [...s.tabs.slice(0, at), tab, ...s.tabs.slice(at)];
}

/** Converts the first saved shape into a tree arrangement (presets and shared ratios). */
function fromV1(saved: ArrangementV1): Arrangement {
  const count = saved.layout === 'single' ? 1 : saved.layout === 'grid' ? 4 : 2;
  const ids = Array.from({ length: count }, (_, i) => `old${i}`);
  let tree = presetTree(saved.layout, ids, newSplitId);
  const apply = (n: LayoutNode): LayoutNode =>
    n.kind === 'group' ? n : { ...n, ratio: clampRatio(n.dir === 'row' ? saved.ratios.x : saved.ratios.y), a: apply(n.a), b: apply(n.b) };
  tree = apply(tree);
  return {
    v: 2,
    tree,
    groups: ids.map((id, i) => ({ id, tabIds: saved.groups[i]?.tabIds ?? [], activeId: saved.groups[i]?.activeId })),
    focused: ids[saved.focused] ?? ids[0]!,
  };
}

function initial() {
  const id = newGroupId();
  return {
    tabs: [] as Tab[],
    activeId: undefined,
    groups: [{ id }] as EditorGroup[],
    focusedGroupId: id,
    groupOf: {} as Record<string, string>,
    tree: { kind: 'group', id } as LayoutNode,
    layout: 'single' as EditorLayout,
    maximizedGroupId: undefined,
    closed: [] as ClosedTab[],
  };
}

export const useTabs = create<TabsState>()((set, get) => {
  /** Applies a change to the group model and re-settles it. */
  const update = (change: (s: Core) => Core | null) =>
    set((s) => {
      const next = change(s);
      return next ? settle(next) : s;
    });

  /** Focus moving to another group ends a maximized view (the other group would be hidden). */
  const focusing = (s: Core, groupId: string): Core =>
    s.maximizedGroupId && s.maximizedGroupId !== groupId ? { ...s, focusedGroupId: groupId, maximizedGroupId: undefined } : { ...s, focusedGroupId: groupId };

  const split = (s: Core, groupId: string, side: Side, tabId?: string): Core | null => {
    if (s.groups.length >= MAX_GROUPS || !s.groups.some((g) => g.id === groupId)) return null;
    // A group's only tab onto its own side would leave the group empty and change nothing.
    if (tabId && s.groupOf[tabId] === groupId && groupTabs(s, groupId).length === 1) return null;
    const id = newGroupId();
    const base: Core = { ...s, tree: splitLeaf(s.tree, groupId, side, id, newSplitId()), groups: [...s.groups, { id }], maximizedGroupId: undefined };
    if (!tabId) return { ...base, focusedGroupId: id };
    return moveInto(base, tabId, id);
  };

  return {
    ...initial(),
    pushClosed: (entry) => set((s) => ({ closed: [...s.closed, entry].slice(-CLOSED_STACK_MAX) })),
    popClosed: () => {
      const entry = get().closed.at(-1);
      if (entry) set((s) => ({ closed: s.closed.slice(0, -1) }));
      return entry;
    },
    setPinned: (id, pinned) =>
      set((s) => {
        const tab = s.tabs.find((t) => t.id === id);
        if (!tab || !!tab.pinned === pinned) return s;
        const rest = s.tabs.filter((t) => t.id !== id);
        const changed = { ...tab, pinned, preview: false } as Tab;
        // Pinned: after the last pinned tab. Unpinned: right after the pinned block.
        const firstUnpinned = rest.findIndex((t) => !t.pinned);
        const at = firstUnpinned < 0 ? rest.length : firstUnpinned;
        return { tabs: [...rest.slice(0, at), changed, ...rest.slice(at)] };
      }),
    open: (tab) =>
      update((s) => {
        const home = s.groupOf[tab.id];
        if (home && s.tabs.some((t) => t.id === tab.id)) return { ...focusing(s, home), groups: setGroupActive(s.groups, home, tab.id) };
        return {
          ...s,
          tabs: insertNew(s, tab, s.focusedGroupId),
          groupOf: { ...s.groupOf, [tab.id]: s.focusedGroupId },
          groups: setGroupActive(s.groups, s.focusedGroupId, tab.id),
        };
      }),
    openPreview: (tab) =>
      update((s) => {
        const home = s.groupOf[tab.id];
        if (home && s.tabs.some((t) => t.id === tab.id)) return { ...focusing(s, home), groups: setGroupActive(s.groups, home, tab.id) };
        const g = s.focusedGroupId;
        const fresh = { ...tab, preview: true } as Tab;
        const old = groupTabs(s, g).find((t) => t.preview);
        // The new preview takes the old one's place in the strip.
        const tabs = old ? s.tabs.map((t) => (t.id === old.id ? fresh : t)) : insertNew(s, fresh, g);
        const groupOf = { ...s.groupOf, [fresh.id]: g };
        if (old) delete groupOf[old.id];
        return { ...s, tabs, groupOf, groups: setGroupActive(s.groups, g, fresh.id) };
      }),
    keepTab: (id) =>
      set((s) => (s.tabs.some((t) => t.id === id && t.preview) ? { tabs: s.tabs.map((t) => (t.id === id ? ({ ...t, preview: false } as Tab) : t)) } : s)),
    activate: (id) =>
      update((s) => {
        const home = s.groupOf[id];
        if (!home || !s.tabs.some((t) => t.id === id)) return null;
        return { ...focusing(s, home), groups: setGroupActive(s.groups, home, id) };
      }),
    focusGroup: (groupId) => update((s) => (s.groups.some((g) => g.id === groupId) ? focusing(s, groupId) : null)),
    focusNeighbour: (step) =>
      update((s) => {
        const at = s.groups.findIndex((g) => g.id === s.focusedGroupId);
        const next = s.groups[(at + step + s.groups.length) % s.groups.length];
        return next ? focusing(s, next.id) : null;
      }),
    canSplit: () => get().groups.length < MAX_GROUPS,
    splitGroup: (groupId, side, tabId) => update((s) => split(s, groupId, side, tabId)),
    splitTab: (tabId, side) =>
      update((s) => {
        const from = s.groupOf[tabId];
        if (!from) return null;
        // Its group's only tab: an empty group opens beside it (one tab is in one group only).
        return groupTabs(s, from).length === 1 ? split(s, from, side) : split(s, from, side, tabId);
      }),
    moveTab: (tabId, groupId, index) => update((s) => moveInto(s, tabId, groupId, index)),
    shiftTab: (step) =>
      update((s) => {
        const id = s.activeId;
        if (!id) return null;
        const members = groupTabs(s, s.focusedGroupId);
        const at = members.findIndex((t) => t.id === id);
        const to = at + step;
        if (at < 0 || to < 0 || to >= members.length) return null;
        // moveInto's index counts the group's other tabs; it keeps the pinned block first.
        return moveInto(s, id, s.focusedGroupId, to);
      }),
    moveToNeighbour: (step) => {
      const s = get();
      if (!s.activeId) return;
      const at = s.groups.findIndex((g) => g.id === s.focusedGroupId);
      const target = s.groups[at + step];
      if (target) s.moveTab(s.activeId, target.id);
    },
    setLayout: (layout) =>
      update((s) => {
        const want = layout === 'single' ? 1 : layout === 'grid' ? MAX_GROUPS : 2;
        const ids = s.groups.slice(0, want).map((g) => g.id);
        while (ids.length < want) ids.push(newGroupId());
        const keep = new Set(ids);
        const last = ids.at(-1)!;
        // Tabs of the groups that go away join the last kept group.
        const groupOf = Object.fromEntries(Object.entries(s.groupOf).map(([t, g]) => [t, keep.has(g) ? g : last]));
        const groups = ids.map((id) => s.groups.find((g) => g.id === id) ?? { id });
        const focusedGroupId = keep.has(s.focusedGroupId) ? s.focusedGroupId : last;
        return { ...s, groups, groupOf, focusedGroupId, tree: presetTree(layout, ids, newSplitId), maximizedGroupId: undefined };
      }),
    dropGroup: (groupId) => update((s) => dropIfEmpty(s, groupId)),
    setSplitRatio: (splitId, ratio) => set((s) => ({ tree: setRatio(s.tree, splitId, ratio) })),
    toggleMaximize: (groupId) =>
      update((s) => {
        const id = groupId ?? s.focusedGroupId;
        if (s.groups.length < 2) return null;
        return s.maximizedGroupId === id ? { ...s, maximizedGroupId: undefined } : { ...s, maximizedGroupId: id, focusedGroupId: id };
      }),
    arrangement: () => {
      const s = get();
      return {
        v: 2,
        tree: s.tree,
        groups: s.groups.map((g) => ({ id: g.id, tabIds: groupTabs(s, g.id).map((t) => t.id), activeId: g.activeId })),
        focused: s.focusedGroupId,
      };
    },
    applyArrangement: (input) => {
      const saved: Arrangement = 'v' in input && input.v === 2 ? input : fromV1(input as ArrangementV1);
      if (!isLayoutNode(saved.tree)) return;
      const s = get();
      const open = new Set(s.tabs.map((t) => t.id));
      const savedIds = new Set(leaves(saved.tree));
      // Fresh ids for the restored groups; only groups that still hold an open tab come back.
      const rename = new Map<string, string>();
      const kept = saved.groups.filter((g) => savedIds.has(g.id)).map((g) => ({ ...g, tabIds: g.tabIds.filter((id) => open.has(id)) }));
      let tree: LayoutNode | null = saved.tree;
      for (const g of kept) {
        if (g.tabIds.length) rename.set(g.id, newGroupId());
        else tree = tree && removeLeaf(tree, g.id);
      }
      for (const id of savedIds) if (!kept.some((g) => g.id === id)) tree = tree && removeLeaf(tree, id);
      if (!tree || rename.size === 0 || leaves(tree).length > MAX_GROUPS) return;
      const relabel = (n: LayoutNode): LayoutNode =>
        n.kind === 'group' ? { kind: 'group', id: rename.get(n.id)! } : { ...n, id: newSplitId(), ratio: clampRatio(n.ratio), a: relabel(n.a), b: relabel(n.b) };
      const nextTree = relabel(tree);
      const groups = kept.filter((g) => rename.has(g.id)).map((g) => ({ id: rename.get(g.id)!, activeId: g.activeId }));
      const groupOf: Record<string, string> = {};
      for (const g of kept) for (const id of g.tabIds) if (rename.has(g.id)) groupOf[id] = rename.get(g.id)!;
      const first = leaves(nextTree)[0]!;
      for (const t of s.tabs) groupOf[t.id] ??= first;
      set(settle({ ...s, tree: nextTree, groups, groupOf, focusedGroupId: rename.get(saved.focused) ?? first, maximizedGroupId: undefined }));
    },
    replaceObjectTabs: (replacements) => update((s) => {
      const tabs = s.tabs.flatMap((tab) => {
        if (tab.kind !== 'object' || !(tab.id in replacements)) return [tab];
        const replacement = replacements[tab.id];
        return replacement ? [replacement] : [];
      });
      const groupOf = { ...s.groupOf };
      for (const [id, replacement] of Object.entries(replacements)) {
        const home = groupOf[id];
        delete groupOf[id];
        if (replacement && home) groupOf[replacement.id] = home;
      }
      const groups = s.groups.map((g) => {
        if (!g.activeId || !(g.activeId in replacements)) return g;
        const replacement = replacements[g.activeId];
        if (replacement) return { ...g, activeId: replacement.id };
        const members = groupTabs(s, g.id);
        const at = members.findIndex((t) => t.id === g.activeId);
        const surviving = (t: Tab) => !(t.id in replacements) || replacements[t.id] !== null;
        const next = members.slice(at + 1).find(surviving) ?? members.slice(0, at).reverse().find(surviving);
        return { ...g, activeId: next ? replacements[next.id]?.id ?? next.id : undefined };
      });
      let next: Core = { ...s, tabs, groupOf, groups };
      for (const g of s.groups) next = dropIfEmpty(next, g.id);
      return next;
    }),
    moveDesignTab: (id, table) => {
      const tab = get().tabs.find((t) => t.id === id);
      if (tab?.kind !== 'design') return null;
      const next = designTabId(tab.connectionId, tab.scope, table);
      if (next === id) {
        if (tab.table !== table || tab.title !== table) set((s) => ({ tabs: s.tabs.map((t) => (t.id === id ? { ...tab, table, title: table } : t)) }));
        return id;
      }
      if (get().tabs.some((t) => t.id === next)) return null;
      update((s) => {
        const groupOf = { ...s.groupOf };
        if (groupOf[id]) groupOf[next] = groupOf[id]!;
        delete groupOf[id];
        return {
          ...s,
          tabs: s.tabs.map((t) => (t.id === id ? { ...tab, id: next, table, title: table } : t)),
          groupOf,
          groups: s.groups.map((g) => (g.activeId === id ? { ...g, activeId: next } : g)),
        };
      });
      return next;
    },
    bind: (id, connectionId) =>
      set((s) => ({ tabs: s.tabs.map((t) => (t.id === id && t.kind === 'query' ? { ...t, connectionId } : t)) })),
    rename: (id, title) =>
      set((s) => (s.tabs.some((t) => t.id === id && t.title !== title) ? { tabs: s.tabs.map((t) => (t.id === id ? { ...t, title } : t)) } : s)),
    close: (id) =>
      set((s) => {
        const home = s.groupOf[id];
        const gone = s.tabs.find((t) => t.id === id);
        if (!home || !gone) return s;
        const members = groupTabs(s, home);
        const at = members.findIndex((t) => t.id === id);
        const tabs = s.tabs.filter((t) => t.id !== id);
        // Object tabs are remembered here (query tabs by `useQueryDocs.closeTab`, which has the document).
        const closed =
          gone.kind === 'object'
            ? [...s.closed, { kind: 'object' as const, tab: { ...gone, pinned: false, preview: false } }].slice(-CLOSED_STACK_MAX)
            : s.closed;
        // Closing the active tab activates its right neighbour in the group, else the left one.
        const wasActive = s.groups.find((g) => g.id === home)?.activeId === id;
        const groups = wasActive ? setGroupActive(s.groups, home, (members[at + 1] ?? members[at - 1])?.id) : s.groups;
        return { closed, ...settle(dropIfEmpty({ ...s, tabs, groups }, home)) };
      }),
    closeConnection: (connectionId) =>
      update((s) => {
        const goes = (t: Tab) => (t.kind === 'object' || t.kind === 'design') && t.connectionId === connectionId;
        const tabs = s.tabs.filter((t) => !goes(t));
        if (tabs.length === s.tabs.length) return null;
        // In each group whose active tab went away: the nearest remaining tab to its left, else the first.
        const groups = s.groups.map((g) => {
          const members = groupTabs(s, g.id);
          if (!members.some((t) => t.id === g.activeId && goes(t))) return g;
          const at = members.findIndex((t) => t.id === g.activeId);
          const before = members.slice(0, at).filter((t) => !goes(t));
          return { ...g, activeId: (before.at(-1) ?? members.find((t) => !goes(t)))?.id };
        });
        let next: Core = { ...s, tabs, groups };
        for (const g of s.groups) next = dropIfEmpty(next, g.id);
        return next;
      }),
    reset: () => set(initial()),
  };
});
