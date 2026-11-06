// What the Object info panel of the right sidebar shows when it is not the tree's selection:
// a table or view picked outside the tree (Ctrl+click on its name in a SQL editor, a foreign
// key followed in the panel), whether the panel is pinned, and the way back.
import { create } from 'zustand';
import type { ObjectRef, Scope } from '../bindings';
import { useLayout } from './layout';
import { objectNodeKey } from './treeModel';
import { useTree } from './tree';

/** A table, view or other object, or a database / schema (`scope`). */
export type InfoTarget = { connectionId: string; object: ObjectRef; scope?: undefined } | { connectionId: string; scope: Scope; object?: undefined };

/** Most objects Back can return to. */
export const HISTORY_MAX = 20;

/** The name of what a target shows: the object, else the schema, else the database. */
export const targetName = (t: InfoTarget) => (t.object ? t.object.name : (t.scope.schema ?? t.scope.database));

const keyOf = (t: InfoTarget) => (t.object ? objectNodeKey(t.object).join('/') : `ns:${t.scope.database}/${t.scope.schema ?? ''}`);
const same = (a: InfoTarget, b: InfoTarget) => a.connectionId === b.connectionId && keyOf(a) === keyOf(b);

interface ObjectInfoState {
  /** Shown instead of the tree's selection until the tree selects something else (unless pinned). */
  target: InfoTarget | null;
  /** Pinned: a selection in the connection tree does not change the panel. */
  pinned: boolean;
  /** What Back returns to, latest last. */
  history: InfoTarget[];
  /** Shows `object` in the Object info panel and opens the panel (a Ctrl+click in an editor). */
  show(connectionId: string, object: ObjectRef): void;
  /** Goes from `from` (what the panel shows now) to `to` (a foreign key's table, a database's table); Back returns. */
  follow(from: InfoTarget, to: InfoTarget): void;
  /** Back to the previous object. */
  back(): void;
  /** Pins `current` (what the panel shows now), or unpins. */
  setPinned(pinned: boolean, current?: InfoTarget | null): void;
  /** The tree selected an object: the panel follows it unless pinned. */
  treeSelected(): void;
  reset(): void;
}

export const useObjectInfo = create<ObjectInfoState>()((set, get) => ({
  target: null,
  pinned: false,
  history: [],
  show: (connectionId, object) => {
    set({ target: { connectionId, object }, history: [] });
    useLayout.getState().openRightActivity('info');
  },
  follow: (from, to) => {
    if (same(from, to)) return;
    set((s) => ({ target: to, history: [...s.history, from].slice(-HISTORY_MAX) }));
  },
  back: () => {
    const { history } = get();
    const previous = history[history.length - 1];
    if (previous) set({ target: previous, history: history.slice(0, -1) });
  },
  setPinned: (pinned, current) => set(pinned ? { pinned, target: get().target ?? current ?? null } : { pinned }),
  treeSelected: () => {
    if (!get().pinned) set({ target: null, history: [] });
  },
  reset: () => set({ target: null, pinned: false, history: [] }),
}));

// A new selection in the tree takes the panel back (unless it is pinned).
useTree.subscribe((s, prev) => {
  if (s.selectedId !== prev.selectedId && (useObjectInfo.getState().target || useObjectInfo.getState().history.length)) {
    useObjectInfo.getState().treeSelected();
  }
});
