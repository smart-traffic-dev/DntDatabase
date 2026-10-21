import { create } from 'zustand';
import type { AppError, DesignOpen, DesignRender, ElementRef, Scope, TableDef } from '../bindings';
import { api, needsReconnect, toAppError } from '../api';
import { useConnections } from './connections';
import { useDialogs } from './dialogs';
import { setDesignDraftsSource } from './queryTabs';
import { useTabs } from './tabs';

/** How long edits settle before the live SQL is asked for. */
export const RENDER_DEBOUNCE_MS = 300;

/** The designer's sections (the segmented control of its header). */
export type DesignSubTab = 'columns' | 'indexes' | 'foreign_keys' | 'checks' | 'options' | 'comment';

export interface DesignState {
  open: DesignOpen | null;
  draft: TableDef | null;
  past: TableDef[];
  future: TableDef[];
  selection: ElementRef | null;
  render: DesignRender | null;
  loading: boolean;
  error: string | null;
  /**
   * The backend session is gone (the connection closed, or the designer was closed behind the tab):
   * the draft as it was at that moment, kept until the user reopens and restores or discards it.
   */
  lostDraft: TableDef | null;
  /** The design that was open when the session was lost: a different `open.design_id` means it was reopened. */
  lostDesignId: string | null;
  /** The section shown in the designer. */
  subTab: DesignSubTab;
  /** The draft of an apply that found the table changed (`plan_stale`), kept for comparison after reopening. */
  staleDraft: TableDef | null;
}

/** Most undo steps kept per design. */
export const UNDO_LIMIT = 100;

/** The backend no longer has this design (or its connection): nothing was sent, the draft is still ours. */
export function isDesignLost(e: AppError): boolean {
  return (e.kind === 'config' && e.field === 'design_id') || needsReconnect(e);
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
  /**
   * Applies `recipe` to the draft (undoable) and schedules the live SQL. Consecutive edits with the
   * same `coalesce` key (typing in one field) make one undo step.
   */
  edit(tabId: string, recipe: (d: TableDef) => TableDef, coalesce?: string): void;
  setSubTab(tabId: string, subTab: DesignSubTab): void;
  undo(tabId: string): void;
  redo(tabId: string): void;
  select(tabId: string, el: ElementRef | null): void;
  /** Throws the changes away: the draft goes back to the opened definition. */
  discard(tabId: string): void;
  /** Opens a fresh session for the same table (after an apply or a stale design). */
  reload(tabId: string): Promise<void>;
  /** The backend session is gone: keep the draft in `lostDraft` and stop rendering. */
  markLost(tabId: string): void;
  /** Reopens a lost design; the old draft stays in `lostDraft` until restored or discarded. */
  reopen(tabId: string): Promise<void>;
  /** Remembers `draft` as the one a stale apply left behind (see `DesignState.staleDraft`). */
  setStaleDraft(tabId: string, draft: TableDef | null): void;
  /** Puts `lostDraft` back as the draft (an undoable edit) once the design is open again. */
  restoreLostDraft(tabId: string): void;
  /** Forgets the tab's design and closes its backend session. */
  dispose(tabId: string): void;
  isDirty(tabId: string): boolean;
  changeCount(tabId: string): number;
  reset(): void;
}

const blank = (): DesignState => ({
  open: null,
  draft: null,
  past: [],
  future: [],
  selection: null,
  render: null,
  loading: false,
  error: null,
  lostDraft: null,
  lostDesignId: null,
  subTab: 'columns',
  staleDraft: null,
});

const timers = new Map<string, ReturnType<typeof setTimeout>>();
/** The `coalesce` key of a tab's last edit, while the next edit may still merge into it. */
const lastCoalesce = new Map<string, string>();
/** Bumped per request of a tab: only the newest answer is kept. */
const sequence = new Map<string, number>();
/** Bumped per open / reopen / reload of a tab (never by renders or edits): only the newest open lands. */
const openSeq = new Map<string, number>();
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

export function deepEqual(a: unknown, b: unknown): boolean {
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
 * removed or modified (matched by `original`); the primary key; the table's name and its comment
 * (one each); its options; and one for a reordering of the columns that stay.
 */
export function countChanges(base: TableDef, draft: TableDef): number {
  let n = listChanges(base.columns, draft.columns) + listChanges(base.indexes, draft.indexes);
  n += listChanges(base.foreign_keys, draft.foreign_keys) + listChanges(base.checks, draft.checks);
  if (!deepEqual(base.primary_key, draft.primary_key)) n++;
  if (base.name !== draft.name) n++;
  if (base.comment !== draft.comment) n++;
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
            if (sequence.get(tabId) !== seq || !entry(tabId)) return;
            const error = toAppError(e);
            if (isDesignLost(error)) get().markLost(tabId);
            else patch(tabId, { error: error.message });
          });
      }, RENDER_DEBOUNCE_MS),
    );
  }

  const closeSession = (designId: string) => {
    void api.tableDesignClose(designId).catch(() => undefined);
  };

  /** Moves the draft to `draft` (undo / redo / edit share this) and refreshes the live SQL. */
  const setDraft = (tabId: string, draft: TableDef, past: TableDef[], future: TableDef[]) => {
    lastCoalesce.delete(tabId);
    patch(tabId, { draft, past, future });
    scheduleRender(tabId);
  };

  /**
   * Opens the design of `target` and swaps it in. The tab keeps its current design (and draft)
   * until the new one is open, and the old backend session closes only after that; a failure
   * keeps the old state and sets `error`. Only a newer open or a dispose supersedes an open
   * (`openSeq`); edits, renders and `markLost` do not touch it, and edits are ignored meanwhile.
   * A `lostDraft` present when the open ends stays (the user's work is never dropped here).
   */
  async function openInto(tabId: string, target: Target) {
    targets.set(tabId, target);
    clearTimer(tabId);
    lastCoalesce.delete(tabId);
    const seq = (openSeq.get(tabId) ?? 0) + 1;
    openSeq.set(tabId, seq);
    const had = entry(tabId)?.open ?? null;
    if (had) patch(tabId, { loading: true, error: null });
    else patch(tabId, { ...blank(), loading: true });
    try {
      const open = await api.tableDesignOpen(target.connectionId, target.scope, target.table);
      // The tab was closed, or another load started, while the session opened.
      if (!entry(tabId) || openSeq.get(tabId) !== seq) {
        closeSession(open.design_id);
        return;
      }
      // Answers still on their way belong to the old design.
      bump(tabId);
      patch(tabId, {
        open,
        draft: open.def,
        past: [],
        future: [],
        selection: null,
        render: null,
        loading: false,
        error: null,
      });
      if (had && had.design_id !== open.design_id) closeSession(had.design_id);
    } catch (e) {
      if (!entry(tabId) || openSeq.get(tabId) !== seq) return;
      patch(tabId, { loading: false, error: toAppError(e).message });
      // The old design stays live: its pending live SQL is asked for again.
      if (had && !entry(tabId)?.lostDraft && get().isDirty(tabId)) scheduleRender(tabId);
    }
  }

  return {
    byTab: {},
    load: (tabId, connectionId, scope, table) => openInto(tabId, { connectionId, scope, table }),
    edit(tabId, recipe, coalesce) {
      const state = entry(tabId);
      // A lost design takes no edits (reopen it first), nor does one that is being (re)opened.
      if (!state?.draft || state.lostDraft || state.loading) return;
      const next = recipe(state.draft);
      if (next === state.draft) return;
      const merge = coalesce !== undefined && lastCoalesce.get(tabId) === coalesce && state.future.length === 0 && state.past.length > 0;
      setDraft(tabId, next, merge ? state.past : [...state.past, state.draft].slice(-UNDO_LIMIT), []);
      if (coalesce !== undefined) lastCoalesce.set(tabId, coalesce);
    },
    setSubTab: (tabId, subTab) => patch(tabId, { subTab }),
    undo(tabId) {
      const state = entry(tabId);
      const previous = state?.past.at(-1);
      if (!state?.draft || !previous || state.loading) return;
      setDraft(tabId, previous, state.past.slice(0, -1), [...state.future, state.draft]);
    },
    redo(tabId) {
      const state = entry(tabId);
      const next = state?.future.at(-1);
      if (!state?.draft || !next || state.loading) return;
      setDraft(tabId, next, [...state.past, state.draft].slice(-UNDO_LIMIT), state.future.slice(0, -1));
    },
    select: (tabId, el) => patch(tabId, { selection: el }),
    discard(tabId) {
      const state = entry(tabId);
      if (!state?.open) return;
      clearTimer(tabId);
      lastCoalesce.delete(tabId);
      bump(tabId);
      patch(tabId, { draft: state.open.def, past: [], future: [], selection: null, render: null, error: null, lostDraft: null, lostDesignId: null });
    },
    async reload(tabId) {
      const target = targets.get(tabId);
      if (target) await openInto(tabId, target);
    },
    markLost(tabId) {
      const state = entry(tabId);
      if (!state?.draft || state.lostDraft) return;
      clearTimer(tabId);
      lastCoalesce.delete(tabId);
      bump(tabId);
      patch(tabId, { lostDraft: state.draft, lostDesignId: state.open?.design_id ?? null, loading: false, error: null });
    },
    async reopen(tabId) {
      const target = targets.get(tabId);
      if (target) await openInto(tabId, target);
    },
    setStaleDraft: (tabId, draft) => patch(tabId, { staleDraft: draft }),
    restoreLostDraft(tabId) {
      const state = entry(tabId);
      const lost = state?.lostDraft;
      if (!state?.draft || !lost || state.loading) return;
      patch(tabId, { lostDraft: null, lostDesignId: null });
      setDraft(tabId, lost, [...state.past, state.draft].slice(-UNDO_LIMIT), []);
    },
    dispose(tabId) {
      clearTimer(tabId);
      lastCoalesce.delete(tabId);
      sequence.delete(tabId);
      openSeq.delete(tabId);
      targets.delete(tabId);
      const designId = entry(tabId)?.open?.design_id;
      set((s) => ({ byTab: Object.fromEntries(Object.entries(s.byTab).filter(([id]) => id !== tabId)) }));
      if (designId) closeSession(designId);
    },
    // A lost design's draft still counts: it is the user's work.
    isDirty(tabId) {
      const state = entry(tabId);
      const draft = state?.lostDraft ?? state?.draft;
      return !!state?.open && !!draft && !deepEqual(draft, state.open.def);
    },
    changeCount(tabId) {
      const state = entry(tabId);
      const draft = state?.lostDraft ?? state?.draft;
      return state?.open && draft ? countChanges(state.open.def, draft) : 0;
    },
    reset() {
      for (const t of timers.values()) clearTimeout(t);
      timers.clear();
      lastCoalesce.clear();
      sequence.clear();
      openSeq.clear();
      targets.clear();
      set({ byTab: {} });
    },
  };
});

/** The ids of the design tabs with unapplied changes. */
export const tabsWithDirtyDesigns = () =>
  Object.keys(useTableDesigns.getState().byTab).filter((id) => useTableDesigns.getState().isDirty(id));

/** The changes that closing `tabIds` would throw away; a dirty design always counts at least one. */
export function designDiscardCount(tabIds: string[]): number {
  const { isDirty, changeCount } = useTableDesigns.getState();
  return tabIds.filter((id) => isDirty(id)).reduce((n, id) => n + Math.max(1, changeCount(id)), 0);
}

/**
 * Asks before the unapplied designs of `tabIds` are thrown away (closing, quitting, a connection
 * going); true to go on. It discards nothing itself: the drafts go when their tabs close, so a
 * later cancelled step (another question, a quit) keeps them.
 */
export function confirmDiscardDesigns(tabIds: string[]): Promise<boolean> {
  const count = designDiscardCount(tabIds);
  if (count === 0) return Promise.resolve(true);
  return new Promise((resolve) => {
    useDialogs.getState().ask({
      kind: 'discardEdits',
      count,
      done: (discard) => {
        useDialogs.getState().answer();
        resolve(discard);
      },
    });
  });
}

/** Asks before one design tab is closed with unapplied changes. */
export const confirmDiscardDesign = (tabId: string) => confirmDiscardDesigns([tabId]);

// Quitting the app asks about unapplied designs too (the close guard lives in `queryTabs`).
setDesignDraftsSource(() => confirmDiscardDesigns(tabsWithDirtyDesigns()));

// A connection that closes or drops takes its designs' backend sessions with it: the drafts stay, marked lost.
useConnections.subscribe((state, prev) => {
  if (state.info === prev.info) return;
  const gone = [...prev.info.keys()].filter((id) => !state.info.has(id));
  if (gone.length === 0) return;
  for (const [tabId, target] of targets) if (gone.includes(target.connectionId)) useTableDesigns.getState().markLost(tabId);
});

// Closing a design tab closes its backend session and forgets the draft.
useTabs.subscribe((state) => {
  const open = new Set(state.tabs.map((t) => t.id));
  for (const id of Object.keys(useTableDesigns.getState().byTab)) {
    if (!open.has(id)) useTableDesigns.getState().dispose(id);
  }
});
