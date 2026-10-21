import { create } from 'zustand';
import { api, toAppError } from '../api';
import { plural } from '../lib/plural';
import type {
  ApplyReport,
  CellInput,
  EditPreview,
  EditSet,
  EditableInfo,
  ObjectRef,
  RowEdit,
  RowRef,
  TableDataPage,
  Value,
} from '../bindings';
import { useDialogs } from './dialogs';
import { setStagedEditsSource } from './queryTabs';
import type { ObjectTabRef as Tab } from './tabs';
import { useTabs } from './tabs';
import { useTableData } from './tableData';

/** The JSON of a row's key (`RowRef.key`): it names a row across page reloads, row numbers do not. */
export type RowId = string;

/** A row staged for INSERT: its typed cells by column (the others take the column default). */
export interface InsertRow {
  id: number;
  values: Map<string, CellInput>;
}

/** The staged changes of one data tab. */
export interface Staged {
  /** Staged cell edits per row, with the row as it was loaded when its first edit was staged. */
  updates: Map<RowId, { ref: RowRef; cells: Map<string, CellInput> }>;
  inserts: InsertRow[];
  /** Rows to delete, as they were loaded when the delete was staged. */
  deletes: Map<RowId, RowRef>;
}

export type StageEdit =
  | { kind: 'update'; row: RowId; ref: RowRef; column: string; input: CellInput }
  | { kind: 'delete'; row: RowId; ref: RowRef }
  | { kind: 'insert' }
  | { kind: 'insertCell'; insert: number; column: string; input: CellInput }
  | { kind: 'dropInsert'; insert: number };

export type StageResult = { ok: true } | { ok: false; message: string };

export const ONE_AT_A_TIME = "This table can't roll back a batch: apply this change first.";
export const STALE_NOTICE = 'The table or connection changed; review the statements again.';
export const UNKNOWN_OUTCOME = 'Outcome unknown: the change may have been applied. Reload to check.';

/** The staged item behind one edit of a plan. */
export type EditKey = { kind: 'update' | 'delete'; row: RowId } | { kind: 'insert'; id: number };

/** The plan the user reviews before anything is sent (`data_preview`), and where that review stands. */
export interface PendingApply {
  preview: EditPreview;
  overwrite: boolean;
  /** What each edit of the plan stands for, to mark a conflict on its row or drop what was applied. */
  keys: EditKey[];
  /** Overwrite of one conflicting row only (null: the whole stage). */
  only: RowId | null;
  /** Set after `plan_stale`: the statements are new. */
  notice: string | null;
  busy: boolean;
  error: string | null;
}

export interface EditState {
  object: ObjectRef | null;
  info: EditableInfo | null;
  staged: Staged;
  /** Earlier states of `staged`, newest last (Undo). */
  history: Staged[];
  insertSeq: number;
  /** Rows the server refused to change because they changed since they were loaded. */
  conflicts: RowId[];
  /** The last apply may or may not have happened. */
  unknown: boolean;
  /** A failure of the last preview / apply, shown as a banner. */
  error: string | null;
  applying: PendingApply | null;
}

const HISTORY_LIMIT = 100;

const emptyStaged = (): Staged => ({ updates: new Map(), inserts: [], deletes: new Map() });

function cloneStaged(s: Staged): Staged {
  return {
    updates: new Map([...s.updates].map(([id, u]) => [id, { ref: u.ref, cells: new Map(u.cells) }])),
    inserts: s.inserts.map((i) => ({ id: i.id, values: new Map(i.values) })),
    deletes: new Map(s.deletes),
  };
}

const countOf = (s: Staged) => s.updates.size + s.inserts.length + s.deletes.size;

const freshState = (object: ObjectRef | null, info: EditableInfo | null): EditState => ({
  object,
  info,
  staged: emptyStaged(),
  history: [],
  insertSeq: 0,
  conflicts: [],
  unknown: false,
  error: null,
  applying: null,
});

/** Why a table is read-only, in words (`EditableInfo.reason` is machine-readable). */
export function readOnlyCopy(reason: string | null): string {
  if (reason === 'no_key') return 'Read-only: this table has no primary key or unique NOT NULL index.';
  if (reason === 'read_only_connection') return 'Read-only connection.';
  if (reason === 'not_a_table') return 'Views are read-only.';
  if (reason?.startsWith('unsupported:')) return `Read-only: ${reason.slice('unsupported:'.length).trim()}`;
  return 'Read-only.';
}

/** The key cells of row `i` of the page (visible and hidden key columns), in `info.key` order; null when one is missing. */
export function rowKeyPairs(page: TableDataPage, info: EditableInfo, i: number): [string, Value][] | null {
  const row = page.rows[i];
  if (!row || info.key.length === 0) return null;
  const pairs: [string, Value][] = [];
  for (const name of info.key) {
    const at = page.columns.findIndex((c) => c.name === name);
    let value: Value | undefined;
    if (at >= 0) value = row[at];
    else {
      const hidden = page.hidden_key.findIndex((c) => c.name === name);
      if (hidden >= 0) value = page.hidden_key_values[i]?.[hidden];
    }
    if (!value) return null;
    pairs.push([name, value]);
  }
  return pairs;
}

/** The id of row `i` of the page; null when the row has no complete key. */
export function rowIdOf(page: TableDataPage, info: EditableInfo, i: number): RowId | null {
  const pairs = rowKeyPairs(page, info, i);
  return pairs ? JSON.stringify(pairs) : null;
}

/** Row `i` of the page as a guarded UPDATE / DELETE needs it. Captured when a change is staged, never rebuilt later. */
export function rowRef(page: TableDataPage, info: EditableInfo, i: number): RowRef | null {
  const key = rowKeyPairs(page, info, i);
  const row = page.rows[i];
  if (!key || !row) return null;
  const isKey = new Set(info.key);
  const original: [string, Value][] = [];
  page.columns.forEach((c, at) => {
    const value = row[at];
    if (value && !isKey.has(c.name)) original.push([c.name, value]);
  });
  return { key, original, xmin: page.row_versions?.[i] ?? null };
}

interface DataEditsStore {
  /** Per data tab id; lives as long as the tab is open. */
  byTab: Record<string, EditState>;
  /** Remembers what the tab shows and how it may be edited (`data_editable`); keeps staged changes. */
  setInfo(tabId: string, object: ObjectRef, info: EditableInfo | null): void;
  stage(tabId: string, edit: StageEdit): StageResult;
  undo(tabId: string): void;
  /** Drops every staged change (and the conflict marks). */
  discard(tabId: string): void;
  hasStaged(tabId: string): boolean;
  /** How many rows have staged changes. */
  changeCount(tabId: string): number;
  /**
   * The staged changes as the `EditSet` `data_preview` takes. Rows are the ones captured when
   * each change was staged (never the page on screen); `only` restricts it to one row.
   */
  toEditSet(tabId: string, overwrite?: boolean, only?: RowId | null): EditSet;
  /** Previews the staged changes; the dialog opens when `applying` is set. `only`: one row (Overwrite). */
  startApply(tab: Tab, opts?: { overwrite?: boolean; only?: RowId }): Promise<void>;
  cancelApply(tabId: string): void;
  /** Sends the previewed plan; the outcome decides what stays staged. */
  confirmApply(tab: Tab): Promise<void>;
  /** Forgets the staged changes of one conflicting row and reads the page again; other rows keep theirs. */
  reloadRow(tab: Tab, row: RowId): void;
  clearBanner(tabId: string): void;
  reset(): void;
}

function build(state: EditState, overwrite: boolean, only: RowId | null): { set: EditSet; keys: EditKey[] } {
  const { object, staged } = state;
  if (!object) throw new Error('data_editable has not answered for this tab');
  const edits: RowEdit[] = [];
  const keys: EditKey[] = [];
  for (const [row, u] of staged.updates) {
    if (only && row !== only) continue;
    edits.push({ op: 'update', row: u.ref, set: [...u.cells] });
    keys.push({ kind: 'update', row });
  }
  for (const [row, ref] of staged.deletes) {
    if (only && row !== only) continue;
    edits.push({ op: 'delete', row: ref });
    keys.push({ kind: 'delete', row });
  }
  if (!only) {
    for (const insert of staged.inserts) {
      edits.push({ op: 'insert', values: [...insert.values] });
      keys.push({ kind: 'insert', id: insert.id });
    }
  }
  return { set: { object, edits, overwrite }, keys };
}

/** `staged` without the items `done` names. */
function without(staged: Staged, done: EditKey[]): Staged {
  const next = cloneStaged(staged);
  for (const k of done) {
    if (k.kind === 'update') next.updates.delete(k.row);
    else if (k.kind === 'delete') next.deletes.delete(k.row);
    else if (k.kind === 'insert') next.inserts = next.inserts.filter((i) => i.id !== k.id);
  }
  return next;
}

export const useDataEdits = create<DataEditsStore>()((set, get) => {
  const patch = (tabId: string, change: Partial<EditState>) =>
    set((s) => {
      const current = s.byTab[tabId];
      return current ? { byTab: { ...s.byTab, [tabId]: { ...current, ...change } } } : s;
    });

  const patchApplying = (tabId: string, change: Partial<PendingApply>) => {
    const current = get().byTab[tabId]?.applying;
    if (current) patch(tabId, { applying: { ...current, ...change } });
  };

  /** Stores the plan of the staged changes for the dialog. `notice` marks a plan made again after `plan_stale`. */
  async function preview(tab: Tab, overwrite: boolean, only: RowId | null, notice: string | null): Promise<void> {
    const state = get().byTab[tab.id];
    if (!state) return;
    try {
      const { set: editSet, keys } = build(state, overwrite, only);
      const made = await api.dataPreview(tab.connectionId, editSet);
      patch(tab.id, { error: null, applying: { preview: made, overwrite, keys, only, notice, busy: false, error: null } });
    } catch (e) {
      const message = toAppError(e).message;
      // While the dialog is open the failure belongs in it; else in the banner.
      if (get().byTab[tab.id]?.applying) patchApplying(tab.id, { busy: false, error: message });
      else patch(tab.id, { error: message });
    }
  }

  function finish(tab: Tab, report: ApplyReport, pending: PendingApply) {
    const state = get().byTab[tab.id];
    if (!state) return;
    const { keys, only } = pending;
    const outcome = report.outcome;
    /** The rows whose statements found the row changed on the server. */
    const conflictRows = () =>
      report.statements
        .filter((s) => s.conflict)
        .map((s) => keys[s.index])
        .filter((k): k is EditKey & { row: RowId } => !!k && k.kind !== 'insert')
        .map((k) => k.row);
    const failed = () => report.statements.find((s) => s.error)?.error ?? report.error;
    if (outcome.outcome === 'committed') {
      if (only) {
        patch(tab.id, {
          staged: without(state.staged, keys),
          history: [],
          conflicts: state.conflicts.filter((r) => r !== only),
          unknown: false,
          error: null,
          applying: null,
        });
      } else {
        patch(tab.id, { staged: emptyStaged(), history: [], conflicts: [], unknown: false, error: null, applying: null });
      }
      void useTableData.getState().load(tab);
    } else if (outcome.outcome === 'rolled_back') {
      const conflicts = conflictRows();
      patch(tab.id, {
        conflicts: only ? [...new Set([...state.conflicts, ...conflicts])] : conflicts,
        applying: null,
        error: conflicts.length > 0 ? null : (failed()?.message ?? 'The changes were rolled back; nothing was applied.'),
      });
    } else if (outcome.outcome === 'partially_applied') {
      // Without a transaction the first statements are applied for good: they must not run again.
      // The rest certainly did not apply: a conflict or the server's error says why (rule 7: no "may have").
      const applied = keys.filter((_, i) => i < outcome.applied);
      const conflicts = conflictRows();
      const reason = failed()?.message ?? (conflicts.length > 0 ? 'a row changed on the server' : 'The changes stopped before all of them were applied.');
      // The applied changes are on the server: say so, and show the page as it is now.
      const ran = applied.length;
      const message = ran > 0 ? `${plural(ran, 'change')} ${ran === 1 ? 'was' : 'were'} applied before the error: ${reason}` : reason;
      patch(tab.id, {
        staged: ran ? without(state.staged, applied) : state.staged,
        history: [],
        conflicts: only ? [...new Set([...state.conflicts, ...conflicts])] : conflicts,
        applying: null,
        error: conflicts.length > 0 && ran === 0 ? null : message,
      });
      if (ran > 0) void useTableData.getState().load(tab);
    } else {
      // Whether anything applied cannot be told: the stage is kept and the user reloads to check.
      patch(tab.id, {
        history: [],
        unknown: true,
        applying: null,
        error: null,
      });
    }
  }

  return {
    byTab: {},
    setInfo(tabId, object, info) {
      set((s) => {
        const current = s.byTab[tabId];
        return { byTab: { ...s.byTab, [tabId]: current ? { ...current, object, info } : freshState(object, info) } };
      });
    },
    stage(tabId, edit) {
      const state = get().byTab[tabId];
      if (!state) return { ok: false, message: 'This table is not ready to edit.' };
      const next = cloneStaged(state.staged);
      let insertSeq = state.insertSeq;
      switch (edit.kind) {
        case 'update': {
          if (next.deletes.has(edit.row)) return { ok: false, message: 'This row is marked for deletion.' };
          // The row is captured by its first edit and stays as it was then.
          const u = next.updates.get(edit.row) ?? { ref: edit.ref, cells: new Map<string, CellInput>() };
          // A cut original fetched in full for this edit: its token lets the backend check that column.
          const fetched = edit.ref.loaded ?? [];
          if (fetched.length > 0) {
            const kept = (u.ref.loaded ?? []).filter(([c]) => !fetched.some(([f]) => f === c));
            u.ref = { ...u.ref, loaded: [...kept, ...fetched] };
          }
          u.cells.set(edit.column, edit.input);
          next.updates.set(edit.row, u);
          break;
        }
        case 'delete': {
          // A delete after edits keeps the row as it was loaded before them.
          const ref = next.updates.get(edit.row)?.ref ?? edit.ref;
          next.updates.delete(edit.row);
          next.deletes.set(edit.row, ref);
          break;
        }
        case 'insert':
          next.inserts.push({ id: ++insertSeq, values: new Map() });
          break;
        case 'insertCell': {
          const row = next.inserts.find((i) => i.id === edit.insert);
          if (!row) return { ok: false, message: 'That new row is gone.' };
          row.values.set(edit.column, edit.input);
          break;
        }
        case 'dropInsert':
          next.inserts = next.inserts.filter((i) => i.id !== edit.insert);
          break;
      }
      // A table that cannot roll back a batch applies one change at a time (the backend refuses more too).
      if (state.info && state.info.atomic !== 'yes' && countOf(next) > 1) return { ok: false, message: ONE_AT_A_TIME };
      patch(tabId, { staged: next, history: [...state.history, state.staged].slice(-HISTORY_LIMIT), insertSeq, error: null });
      return { ok: true };
    },
    undo(tabId) {
      const state = get().byTab[tabId];
      const previous = state?.history.at(-1);
      if (!state || !previous) return;
      patch(tabId, { staged: previous, history: state.history.slice(0, -1) });
    },
    discard(tabId) {
      const state = get().byTab[tabId];
      if (state) set((s) => ({ byTab: { ...s.byTab, [tabId]: freshState(state.object, state.info) } }));
    },
    hasStaged: (tabId) => countOf(get().byTab[tabId]?.staged ?? emptyStaged()) > 0,
    changeCount: (tabId) => countOf(get().byTab[tabId]?.staged ?? emptyStaged()),
    toEditSet(tabId, overwrite = false, only = null) {
      const state = get().byTab[tabId];
      if (!state) throw new Error('no staged changes');
      return build(state, overwrite, only).set;
    },
    async startApply(tab, opts = {}) {
      if (!get().hasStaged(tab.id)) return;
      await preview(tab, opts.overwrite ?? false, opts.only ?? null, null);
    },
    cancelApply(tabId) {
      patch(tabId, { applying: null });
    },
    async confirmApply(tab) {
      const pending = get().byTab[tab.id]?.applying;
      if (!pending || pending.busy) return;
      patchApplying(tab.id, { busy: true, error: null });
      try {
        // The dialog is the confirmation, also of a Production plan.
        const report = await api.dataApply(pending.preview.plan_id, true);
        finish(tab, report, pending);
      } catch (e) {
        const error = toAppError(e);
        if (error.kind === 'plan_stale') {
          // Nothing was sent: show the new statements for another review.
          await preview(tab, pending.overwrite, pending.only, STALE_NOTICE);
        } else {
          patchApplying(tab.id, { busy: false, error: error.message });
        }
      }
    },
    reloadRow(tab, row) {
      const state = get().byTab[tab.id];
      if (!state) return;
      const next = cloneStaged(state.staged);
      next.updates.delete(row);
      next.deletes.delete(row);
      patch(tab.id, { staged: next, history: [], conflicts: state.conflicts.filter((r) => r !== row) });
      void useTableData.getState().load(tab);
    },
    clearBanner(tabId) {
      patch(tabId, { unknown: false, error: null });
    },
    reset() {
      set({ byTab: {} });
    },
  };
});

/** Asks before the staged changes of `tabIds` are thrown away (closing, quitting, a connection going); true to go on. */
export function confirmDiscardMany(tabIds: string[]): Promise<boolean> {
  const { changeCount } = useDataEdits.getState();
  const count = tabIds.reduce((n, id) => n + changeCount(id), 0);
  if (count === 0) return Promise.resolve(true);
  return new Promise((resolve) => {
    useDialogs.getState().ask({
      kind: 'discardEdits',
      count,
      done: (discard) => {
        useDialogs.getState().answer();
        if (discard) for (const id of tabIds) useDataEdits.getState().discard(id);
        resolve(discard);
      },
    });
  });
}

/** Asks before staged changes are thrown away (closing the tab, changing the page or the filter); true to go on. */
export const confirmDiscard = (tabId: string) => confirmDiscardMany([tabId]);

/** The ids of the tabs with staged changes. */
export const tabsWithStaged = () =>
  Object.keys(useDataEdits.getState().byTab).filter((id) => useDataEdits.getState().hasStaged(id));

// Quitting the app asks about staged changes too (the close guard lives in `queryTabs`).
setStagedEditsSource(() => confirmDiscardMany(tabsWithStaged()));

// Forget the changes of closed tabs.
useTabs.subscribe((state) => {
  const open = new Set(state.tabs.map((t) => t.id));
  const { byTab } = useDataEdits.getState();
  const gone = Object.keys(byTab).filter((id) => !open.has(id));
  if (gone.length === 0) return;
  useDataEdits.setState({ byTab: Object.fromEntries(Object.entries(byTab).filter(([id]) => open.has(id))) });
});
