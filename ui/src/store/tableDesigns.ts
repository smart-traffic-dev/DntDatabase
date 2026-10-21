import { create } from 'zustand';
import type { DesignOpen, DesignRender, ElementRef, Scope, TableDef } from '../bindings';
import { api, toAppError } from '../api';
import { useDialogs } from './dialogs';
import { setDesignDraftsSource } from './queryTabs';
import { useTabs } from './tabs';

/** How long edits settle before the live SQL is asked for. */
export const RENDER_DEBOUNCE_MS = 300;

export interface DesignState {
  open: DesignOpen | null;
  draft: TableDef | null;
  past: TableDef[];
  future: TableDef[];
  selection: ElementRef | null;
  render: DesignRender | null;
  loading: boolean;
  error: string | null;
}

interface Target {
  connectionId: string;
  scope: Scope;
  table: string | null;
}

interface DesignsStore {
  byTab: Record<string, DesignState>;
  /** Opens the design session on the backend and starts the draft from its definition. */
  load(tabId: string, connectionId: string, scope: Scope, table: string | null): Promise<void>;
  /** Applies `recipe` to the draft (undoable) and schedules the live SQL. */
  edit(tabId: string, recipe: (d: TableDef) => TableDef): void;
  undo(tabId: string): void;
  redo(tabId: string): void;
  select(tabId: string, el: ElementRef | null): void;
  /** Throws the changes away: the draft goes back to the opened definition. */
  discard(tabId: string): void;
  /** Opens a fresh session for the same table (after an apply or a stale design). */
  reload(tabId: string): Promise<void>;
  /** Forgets the tab's design and closes its backend session. */
  dispose(tabId: string): void;
  isDirty(tabId: string): boolean;
  changeCount(tabId: string): number;
  reset(): void;
}

const blank = (): DesignState => ({ open: null, draft: null, past: [], future: [], selection: null, render: null, loading: false, error: null });

const timers = new Map<string, ReturnType<typeof setTimeout>>();
/** Bumped per request of a tab: only the newest answer is kept. */
const sequence = new Map<string, number>();
const targets = new Map<string, Target>();

const bump = (tabId: string) => {
  const n = (sequence.get(tabId) ?? 0) + 1;
  sequence.set(tabId, n);
  return n;
};

function clearTimer(tabId: string) {
  const t = timers.get(tabId);
  if (t !== undefined) clearTimeout(t);
  timers.delete(tabId);
}

function deepEqual(a: unknown, b: unknown): boolean {
  if (a === b) return true;
  if (typeof a !== 'object' || typeof b !== 'object' || a === null || b === null) return false;
  if (Array.isArray(a) !== Array.isArray(b)) return false;
  const ka = Object.keys(a);
  const kb = Object.keys(b);
  if (ka.length !== kb.length) return false;
  return ka.every((k) => k in b && deepEqual((a as Record<string, unknown>)[k], (b as Record<string, unknown>)[k]));
}

type Element = { original: string | null; name: string };

/** Added + removed + modified elements of one list, matched by `original`. */
function listChanges(base: Element[], draft: Element[]): number {
  const baseByKey = new Map(base.map((b) => [b.original ?? b.name, b]));
  const kept = new Set<string>();
  let n = 0;
  for (const d of draft) {
    const b = d.original === null ? undefined : baseByKey.get(d.original);
    if (!b) n++;
    else {
      kept.add(d.original!);
      if (!deepEqual(d, b)) n++;
    }
  }
  for (const key of baseByKey.keys()) if (!kept.has(key)) n++;
  return n;
}

/**
 * The number of top-level elements that differ: columns, indexes, foreign keys and checks added,
 * removed or modified (matched by `original`); the primary key; the table's own name and comment;
 * its options; and one for a reordering of the columns that stay.
 */
export function countChanges(base: TableDef, draft: TableDef): number {
  let n = listChanges(base.columns, draft.columns) + listChanges(base.indexes, draft.indexes);
  n += listChanges(base.foreign_keys, draft.foreign_keys) + listChanges(base.checks, draft.checks);
  if (!deepEqual(base.primary_key, draft.primary_key)) n++;
  if (base.name !== draft.name || base.comment !== draft.comment) n++;
  if (!deepEqual(base.options, draft.options)) n++;
  const baseOrder = base.columns.map((c) => c.original ?? c.name);
  const keptOrder = draft.columns.filter((c) => c.original !== null && baseOrder.includes(c.original)).map((c) => c.original!);
  if (!deepEqual(baseOrder.filter((k) => keptOrder.includes(k)), keptOrder)) n++;
  return n;
}

export const useTableDesigns = create<DesignsStore>()((set, get) => {
  const patch = (tabId: string, change: Partial<DesignState>) =>
    set((s) => ({ byTab: { ...s.byTab, [tabId]: { ...(s.byTab[tabId] ?? blank()), ...change } } }));
  const entry = (tabId: string) => get().byTab[tabId];

  function scheduleRender(tabId: string) {
    clearTimer(tabId);
    const seq = bump(tabId);
    timers.set(
      tabId,
      setTimeout(() => {
        timers.delete(tabId);
        const state = entry(tabId);
        if (!state?.open || !state.draft) return;
        api
          .tableDesignRender(state.open.design_id, state.draft)
          .then((render) => {
            if (sequence.get(tabId) === seq && entry(tabId)) patch(tabId, { render, error: null });
          })
          .catch((e) => {
            if (sequence.get(tabId) === seq && entry(tabId)) patch(tabId, { error: toAppError(e).message });
          });
      }, RENDER_DEBOUNCE_MS),
    );
  }

  const closeSession = (designId: string) => {
    void api.tableDesignClose(designId).catch(() => undefined);
  };

  /** Moves the draft to `draft` (undo / redo / edit share this) and refreshes the live SQL. */
  const setDraft = (tabId: string, draft: TableDef, past: TableDef[], future: TableDef[]) => {
    patch(tabId, { draft, past, future });
    scheduleRender(tabId);
  };

  return {
    byTab: {},
    async load(tabId, connectionId, scope, table) {
      targets.set(tabId, { connectionId, scope, table });
      clearTimer(tabId);
      const seq = bump(tabId);
      const previous = entry(tabId)?.open?.design_id;
      patch(tabId, { ...blank(), loading: true });
      try {
        const open = await api.tableDesignOpen(connectionId, scope, table);
        // The tab was closed, or another load started, while the session opened.
        if (!entry(tabId) || sequence.get(tabId) !== seq) {
          closeSession(open.design_id);
          return;
        }
        patch(tabId, { open, draft: open.def, loading: false, error: null });
        if (previous && previous !== open.design_id) closeSession(previous);
      } catch (e) {
        if (entry(tabId) && sequence.get(tabId) === seq) patch(tabId, { loading: false, error: toAppError(e).message });
      }
    },
    edit(tabId, recipe) {
      const state = entry(tabId);
      if (!state?.draft) return;
      const next = recipe(state.draft);
      if (next === state.draft) return;
      setDraft(tabId, next, [...state.past, state.draft], []);
    },
    undo(tabId) {
      const state = entry(tabId);
      const previous = state?.past.at(-1);
      if (!state?.draft || !previous) return;
      setDraft(tabId, previous, state.past.slice(0, -1), [...state.future, state.draft]);
    },
    redo(tabId) {
      const state = entry(tabId);
      const next = state?.future.at(-1);
      if (!state?.draft || !next) return;
      setDraft(tabId, next, [...state.past, state.draft], state.future.slice(0, -1));
    },
    select: (tabId, el) => patch(tabId, { selection: el }),
    discard(tabId) {
      const state = entry(tabId);
      if (!state?.open) return;
      clearTimer(tabId);
      bump(tabId);
      patch(tabId, { draft: state.open.def, past: [], future: [], selection: null, render: null, error: null });
    },
    async reload(tabId) {
      const target = targets.get(tabId);
      if (target) await get().load(tabId, target.connectionId, target.scope, target.table);
    },
    dispose(tabId) {
      clearTimer(tabId);
      sequence.delete(tabId);
      targets.delete(tabId);
      const designId = entry(tabId)?.open?.design_id;
      set((s) => ({ byTab: Object.fromEntries(Object.entries(s.byTab).filter(([id]) => id !== tabId)) }));
      if (designId) closeSession(designId);
    },
    isDirty(tabId) {
      const state = entry(tabId);
      return !!state?.open && !!state.draft && !deepEqual(state.draft, state.open.def);
    },
    changeCount(tabId) {
      const state = entry(tabId);
      return state?.open && state.draft ? countChanges(state.open.def, state.draft) : 0;
    },
    reset() {
      for (const t of timers.values()) clearTimeout(t);
      timers.clear();
      sequence.clear();
      targets.clear();
      set({ byTab: {} });
    },
  };
});

/** The ids of the design tabs with unapplied changes. */
export const tabsWithDirtyDesigns = () =>
  Object.keys(useTableDesigns.getState().byTab).filter((id) => useTableDesigns.getState().isDirty(id));

/** Asks before the unapplied designs of `tabIds` are thrown away (closing, quitting); true to go on. */
export function confirmDiscardDesigns(tabIds: string[]): Promise<boolean> {
  const { isDirty, changeCount } = useTableDesigns.getState();
  const dirty = tabIds.filter((id) => isDirty(id));
  if (dirty.length === 0) return Promise.resolve(true);
  // A dirty design always counts at least one change (it asks even when the diff finds none).
  const count = dirty.reduce((n, id) => n + Math.max(1, changeCount(id)), 0);
  return new Promise((resolve) => {
    useDialogs.getState().ask({
      kind: 'discardEdits',
      count,
      done: (discard) => {
        useDialogs.getState().answer();
        if (discard) for (const id of dirty) useTableDesigns.getState().discard(id);
        resolve(discard);
      },
    });
  });
}

/** Asks before one design tab is closed with unapplied changes. */
export const confirmDiscardDesign = (tabId: string) => confirmDiscardDesigns([tabId]);

// Quitting the app asks about unapplied designs too (the close guard lives in `queryTabs`).
setDesignDraftsSource(() => confirmDiscardDesigns(tabsWithDirtyDesigns()));

// Closing a design tab closes its backend session and forgets the draft.
useTabs.subscribe((state) => {
  const open = new Set(state.tabs.map((t) => t.id));
  for (const id of Object.keys(useTableDesigns.getState().byTab)) {
    if (!open.has(id)) useTableDesigns.getState().dispose(id);
  }
});
