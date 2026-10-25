import { create } from 'zustand';
import type { DdlEditOpen, DdlEditReport, ObjectKind, Scope, Strategy } from '../bindings';
import { api, toAppError } from '../api';
import { useConnections } from './connections';
import { useDialogs } from './dialogs';
import { setDdlDraftsSource } from './queryTabs';
import { useTabs } from './tabs';

/** What a DDL editor tab edits: an existing object (`name`), or a new one (`name` null). */
export interface DdlTarget {
  connectionId: string;
  scope: Scope;
  kind: ObjectKind;
  name: string | null;
  /** A trigger's table (a new trigger opened from a table node); null otherwise. */
  table: string | null;
}

export interface DdlEditState {
  target: DdlTarget;
  /** The backend edit; null until it opened. */
  open: DdlEditOpen | null;
  /** The text the edit started from (the server's DDL, or the template of a new object). */
  base: string;
  /** The text in the editor. */
  text: string;
  strategy: Strategy | null;
  loading: boolean;
  error: string | null;
  /** The backend edit is gone (its connection closed): the text stays until the user reopens. */
  lost: boolean;
  /** The object changed on the server since the edit opened (a `plan_stale` preview). */
  stale: boolean;
}

interface DdlEditsStore {
  byTab: Record<string, DdlEditState>;
  /** Opens the backend edit of `target`; base and text become its text. */
  load(tabId: string, target: DdlTarget): Promise<void>;
  setText(tabId: string, text: string): void;
  setStrategy(tabId: string, strategy: Strategy | null): void;
  /** Throws the changes away: text = base. */
  revert(tabId: string): void;
  /** Opens a new backend edit of the same target: the text stays, base becomes the server's text. */
  reopen(tabId: string): Promise<void>;
  markStale(tabId: string): void;
  /** The backend edit is gone (connection closed); the text is kept. */
  markLost(tabId: string): void;
  /** An apply succeeded: base = text = the server's DDL, the target (and tab title) is the object as created. */
  applied(tabId: string, report: DdlEditReport): void;
  /** The object is missing after a failed apply: the edit becomes a create with `text` (not applied). */
  restore(tabId: string, text: string): void;
  /** Forgets the tab's edit and closes its backend edit. */
  dispose(tabId: string): void;
  isDirty(tabId: string): boolean;
  /** The open tab that edits `target` (existing objects only), else null. */
  findTab(target: DdlTarget): string | null;
  reset(): void;
}

const blank = (target: DdlTarget): DdlEditState => ({
  target,
  open: null,
  base: '',
  text: '',
  strategy: null,
  loading: false,
  error: null,
  lost: false,
  stale: false,
});

/** Bumped per open of a tab: only the newest open lands. */
const openSeq = new Map<string, number>();

const sameTarget = (a: DdlTarget, b: DdlTarget) =>
  a.connectionId === b.connectionId &&
  a.scope.database === b.scope.database &&
  (a.scope.schema ?? null) === (b.scope.schema ?? null) &&
  a.kind === b.kind &&
  a.name === b.name;

const closeEdit = (editId: string) => {
  void api.ddlEditClose(editId).catch(() => undefined);
};

export const useDdlEdits = create<DdlEditsStore>()((set, get) => {
  const entry = (tabId: string) => get().byTab[tabId];
  const patch = (tabId: string, change: Partial<DdlEditState>) =>
    set((s) => {
      const had = s.byTab[tabId];
      return had ? { byTab: { ...s.byTab, [tabId]: { ...had, ...change } } } : s;
    });

  /**
   * Opens the backend edit of the tab's target. `keepText`: the editor text stays (reopen), else it
   * becomes the server's text too. The old backend edit closes once the new one is open.
   */
  async function openInto(tabId: string, keepText: boolean) {
    const state = entry(tabId);
    if (!state) return;
    const seq = (openSeq.get(tabId) ?? 0) + 1;
    openSeq.set(tabId, seq);
    const had = state.open;
    patch(tabId, { loading: true, error: null });
    const { connectionId, scope, kind, name, table } = state.target;
    try {
      const open = await api.ddlEditOpen(connectionId, scope, kind, name, table);
      if (!entry(tabId) || openSeq.get(tabId) !== seq) {
        closeEdit(open.edit_id);
        return;
      }
      const current = entry(tabId)!;
      patch(tabId, {
        open,
        base: open.text,
        text: keepText ? current.text : open.text,
        strategy: keepText && current.strategy && open.strategies.includes(current.strategy) ? current.strategy : open.strategy,
        loading: false,
        error: null,
        lost: false,
        stale: false,
      });
      if (had && had.edit_id !== open.edit_id) closeEdit(had.edit_id);
    } catch (e) {
      if (!entry(tabId) || openSeq.get(tabId) !== seq) return;
      patch(tabId, { loading: false, error: toAppError(e).message });
    }
  }

  return {
    byTab: {},
    load(tabId, target) {
      set((s) => ({ byTab: { ...s.byTab, [tabId]: blank(target) } }));
      return openInto(tabId, false);
    },
    setText: (tabId, text) => patch(tabId, { text }),
    setStrategy: (tabId, strategy) => patch(tabId, { strategy }),
    revert(tabId) {
      const state = entry(tabId);
      if (state) patch(tabId, { text: state.base });
    },
    // A tab whose first open failed has no text of its own yet: it takes the server's.
    reopen: (tabId) => openInto(tabId, !!entry(tabId)?.open),
    markStale: (tabId) => patch(tabId, { stale: true }),
    markLost(tabId) {
      const state = entry(tabId);
      if (!state || state.lost) return;
      openSeq.set(tabId, (openSeq.get(tabId) ?? 0) + 1);
      patch(tabId, { lost: true, loading: false });
    },
    applied(tabId, report) {
      const state = entry(tabId);
      if (!state) return;
      const text = report.text ?? state.text;
      const name = report.object?.name ?? state.target.name;
      patch(tabId, { base: text, text, stale: false, target: { ...state.target, name } });
      if (name) useTabs.getState().rename(tabId, name);
    },
    restore(tabId, text) {
      const state = entry(tabId);
      if (!state) return;
      patch(tabId, { base: '', text, target: { ...state.target, name: null } });
    },
    dispose(tabId) {
      const editId = entry(tabId)?.open?.edit_id;
      openSeq.delete(tabId);
      set((s) => ({ byTab: Object.fromEntries(Object.entries(s.byTab).filter(([id]) => id !== tabId)) }));
      if (editId) closeEdit(editId);
    },
    isDirty(tabId) {
      const state = entry(tabId);
      return !!state && state.text !== state.base;
    },
    findTab(target) {
      if (target.name === null) return null;
      const found = Object.entries(get().byTab).find(([, s]) => s.target.name !== null && sameTarget(s.target, target));
      return found ? found[0] : null;
    },
    reset() {
      openSeq.clear();
      set({ byTab: {} });
    },
  };
});

/** The ids of the DDL editor tabs with unapplied text. */
export const tabsWithDirtyDdl = () => Object.keys(useDdlEdits.getState().byTab).filter((id) => useDdlEdits.getState().isDirty(id));

/** How many of `tabIds` hold unapplied DDL text (one change each). */
export const ddlDiscardCount = (tabIds: string[]): number => tabIds.filter((id) => useDdlEdits.getState().isDirty(id)).length;

/**
 * Asks before the unapplied DDL text of `tabIds` is thrown away (closing, quitting, a connection
 * going); true to go on. It discards nothing itself: the text goes when its tab closes.
 */
export function confirmDiscardDdl(tabIds: string[]): Promise<boolean> {
  const count = ddlDiscardCount(tabIds);
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

// Quitting the app asks about unapplied DDL text too (the close guard lives in `queryTabs`).
setDdlDraftsSource(() => confirmDiscardDdl(tabsWithDirtyDdl()));

// A connection that closes or drops takes its backend edits with it: the text stays, marked lost.
useConnections.subscribe((state, prev) => {
  if (state.info === prev.info) return;
  const gone = [...prev.info.keys()].filter((id) => !state.info.has(id));
  if (gone.length === 0) return;
  for (const [tabId, s] of Object.entries(useDdlEdits.getState().byTab)) {
    if (gone.includes(s.target.connectionId)) useDdlEdits.getState().markLost(tabId);
  }
});

// Closing a DDL tab closes its backend edit and forgets the text.
useTabs.subscribe((state) => {
  const open = new Set(state.tabs.map((t) => t.id));
  for (const id of Object.keys(useDdlEdits.getState().byTab)) {
    if (!open.has(id)) useDdlEdits.getState().dispose(id);
  }
});
