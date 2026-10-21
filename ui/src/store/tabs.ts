import { create } from 'zustand';
import type { ObjectRef } from '../bindings';
import type { QueryDoc } from './queryTabs';

export interface ObjectTabRef {
  id: string;
  kind: 'object';
  connectionId: string;
  objectRef: ObjectRef;
  title: string;
  /** Pinned tabs sort first and are skipped by "Close others" / "Close tabs to the right". */
  pinned?: boolean;
}

/** A query tab; its document lives in `useQueryDocs` under the same id. */
export interface QueryTabRef {
  id: string;
  kind: 'query';
  connectionId: string | null;
  title: string;
  pinned?: boolean;
}

export type Tab = ObjectTabRef | QueryTabRef;

/** What "Reopen closed tab" brings back: an object tab by reference, a query tab with its document. */
export type ClosedTab = { kind: 'object'; tab: ObjectTabRef } | { kind: 'query'; doc: QueryDoc };

/** How many closed tabs are remembered. */
export const CLOSED_STACK_MAX = 10;

/** How the editor groups share the workspace: 1 group, 2 side by side / stacked, 3–4 in a 2 × 2 grid. */
export type EditorLayout = 'single' | 'columns' | 'rows' | 'grid';

/** A pane of the workspace with its own tab strip. Its tabs are the tabs whose `groupOf` is its id. */
export interface EditorGroup {
  id: string;
  activeId?: string;
}

/** What is saved across restarts: the layout and which tabs sat in which group. */
export interface Arrangement {
  layout: EditorLayout;
  ratios: { x: number; y: number };
  groups: { tabIds: string[]; activeId?: string }[];
  focused: number;
}

export const MAX_GROUPS = 4;
/** No split may squeeze a group below 15 % of the workspace. */
const RATIO_MIN = 0.15;

interface TabsState {
  /** Every open tab, in strip order; each sits in exactly one group (`groupOf`). */
  tabs: Tab[];
  /** The active tab of the focused group (what Run, the shortcuts, the status bar and the title act on). */
  activeId?: string;
  groups: EditorGroup[];
  focusedGroupId: string;
  /** Tab id → group id. */
  groupOf: Record<string, string>;
  layout: EditorLayout;
  /** x: width of the left column, y: height of the top row (fractions of the workspace). */
  ratios: { x: number; y: number };
  focusGroup(groupId: string): void;
  /** Moves `tabId` into a new group to the right / below (an empty one when it is its group's only tab). */
  splitTab(tabId: string, direction: 'right' | 'down'): void;
  /** A new group can still be added. */
  canSplit(): boolean;
  /** Moves a tab into `groupId` (at `index` among that group's tabs, else last); it becomes active there. */
  moveTab(tabId: string, groupId: string, index?: number): void;
  /** Moves the active tab to the next (`1`) / previous (`-1`) group. */
  moveToNeighbour(step: 1 | -1): void;
  /** Focuses the next / previous group, wrapping around. */
  focusNeighbour(step: 1 | -1): void;
  setLayout(layout: EditorLayout): void;
  setRatios(ratios: Partial<{ x: number; y: number }>): void;
  arrangement(): Arrangement;
  /** Puts the open tabs back into a saved arrangement (tabs it does not know go to the first group). */
  applyArrangement(saved: Arrangement): void;
  /** Last closed tabs, most recent last (at most `CLOSED_STACK_MAX`). */
  closed: ClosedTab[];
  /** Remembers a closed tab (called by whoever closes a query tab; object tabs record themselves). */
  pushClosed(entry: ClosedTab): void;
  /** Takes the most recently closed tab off the stack. */
  popClosed(): ClosedTab | undefined;
  /** Pins or unpins a tab: pinned tabs stay at the left. */
  setPinned(id: string, pinned: boolean): void;
  /** Opens a tab in the focused group; a tab that is already open is activated in its own group. */
  open(tab: Tab): void;
  /** Activates a tab and focuses its group. */
  activate(id: string): void;
  /** Rebinds a query tab to another connection (or none). */
  bind(id: string, connectionId: string | null): void;
  rename(id: string, title: string): void;
  close(id: string): void;
  /** Closes every object tab of a connection (it was deleted or replaced); query tabs keep their binding. */
  closeConnection(connectionId: string): void;
  reset(): void;
}

/** One tab per object of a connection. */
export function objectTabId(connectionId: string, o: ObjectRef): string {
  return [connectionId, o.database, o.schema ?? '', o.kind, o.name].map(encodeURIComponent).join('/');
}

/** The tabs of one group, in strip order. */
export const groupTabs = (s: Pick<TabsState, 'tabs' | 'groupOf'>, groupId: string): Tab[] =>
  s.tabs.filter((t) => s.groupOf[t.id] === groupId);

let groupSeq = 0;
const newGroup = (): EditorGroup => ({ id: `g${++groupSeq}` });

type Core = Pick<TabsState, 'tabs' | 'groups' | 'focusedGroupId' | 'groupOf' | 'layout' | 'activeId'>;

/** The layout a number of groups allows: `prefer` when it fits, else the natural one. */
function layoutFor(count: number, prefer: EditorLayout): EditorLayout {
  if (count <= 1) return 'single';
  if (count === 2) return prefer === 'rows' ? 'rows' : 'columns';
  return 'grid';
}

/**
 * Restores the invariants after a change: every tab in one existing group, a focused group,
 * each group's active tab one of its own, the layout matching the group count, and `activeId`
 * the focused group's active tab.
 */
function settle(s: Core): Core {
  const groups = s.groups.length ? s.groups : [newGroup()];
  const focusedGroupId = groups.some((g) => g.id === s.focusedGroupId) ? s.focusedGroupId : groups[0]!.id;
  const known = new Set(groups.map((g) => g.id));
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
    layout: layoutFor(fixed.length, s.layout),
    activeId: focused?.activeId,
  };
}

/** Removes `groupId` when it holds no tab and is not the last group; its neighbour takes the focus. */
function dropIfEmpty(s: Core, groupId: string): Core {
  if (s.groups.length < 2 || s.tabs.some((t) => s.groupOf[t.id] === groupId)) return s;
  const at = s.groups.findIndex((g) => g.id === groupId);
  const groups = s.groups.filter((g) => g.id !== groupId);
  const focusedGroupId = s.focusedGroupId === groupId ? (groups[Math.max(0, at - 1)] ?? groups[0])!.id : s.focusedGroupId;
  return { ...s, groups, focusedGroupId };
}

const setGroupActive = (groups: EditorGroup[], groupId: string, activeId: string | undefined) =>
  groups.map((g) => (g.id === groupId ? { ...g, activeId } : g));

const clampRatio = (v: number) => Math.min(1 - RATIO_MIN, Math.max(RATIO_MIN, v));

function initial() {
  const group = newGroup();
  return {
    tabs: [] as Tab[],
    activeId: undefined,
    groups: [group],
    focusedGroupId: group.id,
    groupOf: {} as Record<string, string>,
    layout: 'single' as EditorLayout,
    ratios: { x: 0.5, y: 0.5 },
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
        const changed = { ...tab, pinned } as Tab;
        // Pinned: after the last pinned tab. Unpinned: right after the pinned block.
        const firstUnpinned = rest.findIndex((t) => !t.pinned);
        const at = firstUnpinned < 0 ? rest.length : firstUnpinned;
        return { tabs: [...rest.slice(0, at), changed, ...rest.slice(at)] };
      }),
    open: (tab) =>
      update((s) => {
        const home = s.groupOf[tab.id];
        if (home && s.tabs.some((t) => t.id === tab.id)) {
          return { ...s, focusedGroupId: home, groups: setGroupActive(s.groups, home, tab.id) };
        }
        return {
          ...s,
          tabs: [...s.tabs, tab],
          groupOf: { ...s.groupOf, [tab.id]: s.focusedGroupId },
          groups: setGroupActive(s.groups, s.focusedGroupId, tab.id),
        };
      }),
    activate: (id) =>
      update((s) => {
        const home = s.groupOf[id];
        if (!home || !s.tabs.some((t) => t.id === id)) return null;
        return { ...s, focusedGroupId: home, groups: setGroupActive(s.groups, home, id) };
      }),
    focusGroup: (groupId) => update((s) => (s.groups.some((g) => g.id === groupId) ? { ...s, focusedGroupId: groupId } : null)),
    focusNeighbour: (step) =>
      update((s) => {
        const at = s.groups.findIndex((g) => g.id === s.focusedGroupId);
        const next = s.groups[(at + step + s.groups.length) % s.groups.length];
        return next ? { ...s, focusedGroupId: next.id } : null;
      }),
    canSplit: () => get().groups.length < MAX_GROUPS,
    splitTab: (tabId, direction) =>
      update((s) => {
        const from = s.groupOf[tabId];
        if (!from || s.groups.length >= MAX_GROUPS) return null;
        const group = newGroup();
        const at = s.groups.findIndex((g) => g.id === from);
        const groups = [...s.groups.slice(0, at + 1), group, ...s.groups.slice(at + 1)];
        const prefer: EditorLayout = s.groups.length === 1 ? (direction === 'right' ? 'columns' : 'rows') : s.layout;
        const alone = groupTabs(s, from).length === 1;
        if (alone) return { ...s, groups, focusedGroupId: group.id, layout: prefer };
        const base = { ...s, groups, layout: prefer, groupOf: { ...s.groupOf, [tabId]: group.id } };
        return { ...base, focusedGroupId: group.id, groups: setGroupActive(base.groups, group.id, tabId) };
      }),
    moveTab: (tabId, groupId, index) =>
      update((s) => {
        const from = s.groupOf[tabId];
        const tab = s.tabs.find((t) => t.id === tabId);
        if (!from || !tab || !s.groups.some((g) => g.id === groupId)) return null;
        // Strip order: place the tab before the target group's tab at `index` (else after its last tab).
        const rest = s.tabs.filter((t) => t.id !== tabId);
        const targets = rest.filter((t) => s.groupOf[t.id] === groupId);
        const anchor = index !== undefined ? targets[index] : undefined;
        const at = anchor ? rest.indexOf(anchor) : targets.length ? rest.indexOf(targets.at(-1)!) + 1 : rest.length;
        const tabs = [...rest.slice(0, at), tab, ...rest.slice(at)];
        // The source group's active tab moves on to a neighbour.
        const left = groupTabs({ tabs: s.tabs, groupOf: s.groupOf }, from);
        const i = left.findIndex((t) => t.id === tabId);
        const neighbour = (left[i + 1] ?? left[i - 1])?.id;
        let groups = setGroupActive(s.groups, groupId, tabId);
        if (from !== groupId && s.groups.find((g) => g.id === from)?.activeId === tabId) groups = setGroupActive(groups, from, neighbour);
        const moved: Core = { ...s, tabs, groups, groupOf: { ...s.groupOf, [tabId]: groupId }, focusedGroupId: groupId };
        return dropIfEmpty(moved, from);
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
        let groups = s.groups.slice(0, want);
        while (groups.length < want) groups = [...groups, newGroup()];
        const keep = new Set(groups.map((g) => g.id));
        const last = groups.at(-1)!.id;
        // Tabs of the groups that go away join the last kept group.
        const groupOf = Object.fromEntries(Object.entries(s.groupOf).map(([t, g]) => [t, keep.has(g) ? g : last]));
        const focusedGroupId = keep.has(s.focusedGroupId) ? s.focusedGroupId : last;
        return { ...s, groups, groupOf, focusedGroupId, layout };
      }),
    setRatios: (r) =>
      set((s) => ({
        ratios: { x: r.x === undefined ? s.ratios.x : clampRatio(r.x), y: r.y === undefined ? s.ratios.y : clampRatio(r.y) },
      })),
    arrangement: () => {
      const s = get();
      return {
        layout: s.layout,
        ratios: s.ratios,
        groups: s.groups.map((g) => ({ tabIds: groupTabs(s, g.id).map((t) => t.id), activeId: g.activeId })),
        focused: Math.max(0, s.groups.findIndex((g) => g.id === s.focusedGroupId)),
      };
    },
    applyArrangement: (saved) => {
      const s = get();
      const open = new Set(s.tabs.map((t) => t.id));
      // Only groups that still hold an open tab come back.
      const kept = saved.groups
        .slice(0, MAX_GROUPS)
        .map((g, i) => ({ ...g, index: i, tabIds: g.tabIds.filter((id) => open.has(id)) }))
        .filter((g) => g.tabIds.length > 0);
      if (kept.length === 0) {
        set({ ratios: { x: clampRatio(saved.ratios.x), y: clampRatio(saved.ratios.y) } });
        return;
      }
      const groups = kept.map((g) => ({ ...newGroup(), activeId: g.activeId }));
      const groupOf: Record<string, string> = {};
      kept.forEach((g, i) => g.tabIds.forEach((id) => (groupOf[id] = groups[i]!.id)));
      for (const t of s.tabs) groupOf[t.id] ??= groups[0]!.id;
      const focusedAt = kept.findIndex((g) => g.index === saved.focused);
      set({
        ratios: { x: clampRatio(saved.ratios.x), y: clampRatio(saved.ratios.y) },
        ...settle({ ...s, groups, groupOf, focusedGroupId: groups[Math.max(0, focusedAt)]!.id, layout: saved.layout }),
      });
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
          gone.kind === 'object' ? [...s.closed, { kind: 'object' as const, tab: { ...gone, pinned: false } }].slice(-CLOSED_STACK_MAX) : s.closed;
        // Closing the active tab activates its right neighbour in the group, else the left one.
        const wasActive = s.groups.find((g) => g.id === home)?.activeId === id;
        const groups = wasActive ? setGroupActive(s.groups, home, (members[at + 1] ?? members[at - 1])?.id) : s.groups;
        return { closed, ...settle(dropIfEmpty({ ...s, tabs, groups }, home)) };
      }),
    closeConnection: (connectionId) =>
      update((s) => {
        const goes = (t: Tab) => t.kind === 'object' && t.connectionId === connectionId;
        const tabs = s.tabs.filter((t) => !goes(t));
        if (tabs.length === s.tabs.length) return null;
        // In each group whose active tab went away: the nearest remaining tab to its left, else the first.
        let groups = s.groups.map((g) => {
          const members = groupTabs(s, g.id);
          if (!members.some((t) => t.id === g.activeId && goes(t))) return g;
          const at = members.findIndex((t) => t.id === g.activeId);
          const before = members.slice(0, at).filter((t) => !goes(t));
          return { ...g, activeId: (before.at(-1) ?? members.find((t) => !goes(t)))?.id };
        });
        let next: Core = { ...s, tabs, groups };
        for (const g of s.groups) next = dropIfEmpty(next, g.id);
        groups = next.groups;
        return { ...next, groups };
      }),
    reset: () => set(initial()),
  };
});
