import { create } from 'zustand';
import { api, toAppError } from '../api';
import type { Dialect, QueryTabRecord, TxAction, TxState } from '../bindings';
import type { Engine } from '../query/engine';
import { useConnections } from './connections';
import { useDialogs } from './dialogs';
import { useTabs } from './tabs';

export type RowLimit = 1000 | 10000 | 100000 | 1000000;
export const ROW_LIMITS: RowLimit[] = [1000, 10000, 100000, 1000000];
export const DEFAULT_ROW_LIMIT: RowLimit = 10000;

/** Debounce of the tab autosave (spec §7.2). */
export const AUTOSAVE_MS = 1000;

// The SQL engine of a document follows its connection. The backend `Dialect` (with the MySQL
// `sql_mode` flags) is `engineDialect(engine)` before a plan, `PlanView.dialect` after one.

/** The backend `Dialect` of an engine, without `sql_mode` flags (what `statement_ranges` takes). */
export const engineDialect = (engine: Engine): Dialect =>
  engine === 'postgres' ? { engine: 'postgres' } : { engine: 'mysql' };

/** The editor document of a query tab, persisted in `query_tabs` (spec §7.2). */
export interface QueryDoc {
  id: string;
  title: string;
  connectionId: string | null;
  database: string | null;
  sql: string;
  /** UTF-16 offset of the cursor. */
  cursor: number;
  filePath: string | null;
  /** The text differs from the file at `filePath` (always false without a file). */
  dirty: boolean;
  continueOnError: boolean;
  rowLimit: RowLimit;
  sortOrder: number;
  dialect: Engine;
}

interface QueryDocsState {
  docs: Map<string, QueryDoc>;
  /** Creates the document and its tab, returns the id. */
  newQuery(connectionId: string | null, database: string | null): string;
  /** Changes a document and schedules its autosave (`AUTOSAVE_MS`). */
  update(id: string, patch: Partial<QueryDoc>): void;
  /** Brings a closed document back as a tab (same text, file and settings); returns its id. */
  reopen(doc: QueryDoc): string;
  /** Drops the document from memory only (no backend call). */
  remove(id: string): void;
  /**
   * Opens the tabs saved by `tabs_save`. Never connects. `connectionsKnown: false` (the tree
   * failed to load the stored connections) keeps every binding, since none can be checked.
   */
  restore(opts?: { connectionsKnown?: boolean }): Promise<void>;
  /** Saves every tab whose autosave is pending, now. */
  /** Saves every pending (or previously failed) tab now; resolves with the ids that could not be saved. */
  flush(): Promise<string[]>;
  /** Closes a query tab after its guards (open transaction, unsaved file); false when it stays. */
  closeTab(id: string): Promise<boolean>;
  /** Asks for a .sql file and opens it in a new tab. */
  openFile(): Promise<void>;
  /** Writes the tab to its file, asking for a path first when it has none (or `saveAs`). */
  saveFile(id: string, saveAs?: boolean): Promise<void>;
  reset(): void;
}

/** "Query N", N = 1 + the highest N among existing documents. */
function nextTitle(docs: Iterable<QueryDoc>): string {
  let highest = 0;
  for (const d of docs) {
    const m = /^Query (\d+)$/.exec(d.title);
    if (m) highest = Math.max(highest, Number(m[1]));
  }
  return `Query ${highest + 1}`;
}

/** The last component of a Windows or POSIX path. */
export const fileName = (path: string): string => path.split(/[\\/]/).filter(Boolean).at(-1) ?? path;

export const dialectOfConnection = (connectionId: string | null): Engine =>
  connectionId && useConnections.getState().summaries.get(connectionId)?.config.kind === 'postgres'
    ? 'postgres'
    : 'mysql';

const asRowLimit = (n: number): RowLimit => (ROW_LIMITS as number[]).includes(n) ? (n as RowLimit) : DEFAULT_ROW_LIMIT;

export function toRecord(doc: QueryDoc): QueryTabRecord {
  return {
    id: doc.id,
    title: doc.title,
    sort_order: doc.sortOrder,
    connection_id: doc.connectionId,
    database: doc.database,
    sql_text: doc.sql,
    cursor: doc.cursor,
    file_path: doc.filePath,
    dirty: doc.filePath !== null && doc.dirty,
    continue_on_error: doc.continueOnError,
    row_limit: doc.rowLimit,
    // Set by `tabs_save`.
    updated_at: '',
  };
}

export function fromRecord(r: QueryTabRecord): QueryDoc {
  return {
    id: r.id,
    title: r.title,
    connectionId: r.connection_id,
    database: r.database,
    sql: r.sql_text,
    cursor: r.cursor,
    filePath: r.file_path,
    dirty: r.file_path !== null && r.dirty,
    continueOnError: r.continue_on_error,
    rowLimit: asRowLimit(r.row_limit),
    sortOrder: r.sort_order,
    dialect: dialectOfConnection(r.connection_id),
  };
}

/**
 * Where the transaction state of a tab comes from: the run store (`useQueryRun().get(id).txState`)
 * registers itself here. Without a source every tab counts as idle, and closing a tab still
 * catches an open transaction through `query_close_tab`'s `transaction_open`.
 */
let txStateOf: (tabId: string) => TxState = () => 'idle';
/** Where the state a `query_tx` of the close guard leaves goes (the run store, as well). */
let recordTxState: (tabId: string, state: TxState) => void = () => {};

export function setTxStateSource(
  source: ((tabId: string) => TxState) | null,
  record?: (tabId: string, state: TxState) => void,
) {
  txStateOf = source ?? (() => 'idle');
  recordTxState = (source && record) || (() => {});
}

/**
 * Whether a tab has a run in progress, and how to stop it: `stop` asks for the stop and resolves
 * once the run ended (true) or after a bounded wait (false). The run flow registers itself here.
 */
/**
 * Staged data edits of the object tabs (registered by `dataEdits`, which this store cannot import):
 * `confirm` asks "Discard N unsaved changes?" when there are any and answers whether to go on.
 */
const stagedEdits: { confirm(): Promise<boolean> } = { confirm: () => Promise.resolve(true) };
export function setStagedEditsSource(confirm: (() => Promise<boolean>) | null) {
  stagedEdits.confirm = confirm ?? (() => Promise.resolve(true));
}

let runningOf: (tabId: string) => boolean = () => false;
let stopRunOf: (tabId: string) => Promise<boolean> = async () => true;
export function setRunningSource(source: ((tabId: string) => boolean) | null, stop?: (tabId: string) => Promise<boolean>) {
  runningOf = source ?? (() => false);
  stopRunOf = stop ?? (async () => true);
}

function confirmStopRun(title: string): Promise<boolean> {
  return new Promise((resolve) => {
    useDialogs.getState().ask({
      kind: 'stopRun',
      title,
      done: (close) => {
        useDialogs.getState().answer();
        resolve(close);
      },
    });
  });
}

const TX_STATES: readonly string[] = ['idle', 'active', 'aborted'] satisfies TxState[];

/** `AppError.tx_state` (a plain string) as a `TxState`; null when absent or unknown. */
export const asTxState = (s: string | null | undefined): TxState | null =>
  s != null && TX_STATES.includes(s) ? (s as TxState) : null;

/** `query_tx`, recording the state it leaves (or the one its failure reports). */
export async function queryTxRecorded(tabId: string, action: TxAction): Promise<TxState> {
  try {
    const state = await api.queryTx(tabId, action);
    recordTxState(tabId, state);
    return state;
  } catch (e) {
    const state = asTxState(toAppError(e).tx_state);
    if (state) recordTxState(tabId, state);
    throw e;
  }
}

const timers = new Map<string, ReturnType<typeof setTimeout>>();
const inflight = new Map<string, Promise<boolean>>();
/** Tabs whose last save failed: `flush` tries them again. */
const unsaved = new Set<string>();
const closing = new Set<string>();

/**
 * Saves one tab now; resolves `false` when `tabs_save` failed (internal errors also reach the
 * toast). A failed tab is remembered and saved again by the next autosave or `flush`.
 */
function persist(id: string): Promise<boolean> {
  const timer = timers.get(id);
  if (timer !== undefined) clearTimeout(timer);
  timers.delete(id);
  if (!useQueryDocs.getState().docs.has(id)) {
    unsaved.delete(id);
    return Promise.resolve(true);
  }
  const previous = inflight.get(id) ?? Promise.resolve(true);
  const p = previous
    .then(() => {
      // Saves the document as it is when its turn comes.
      const doc = useQueryDocs.getState().docs.get(id);
      return doc ? api.tabsSave(toRecord(doc)) : null;
    })
    .then(
      () => {
        unsaved.delete(id);
        return true;
      },
      () => {
        if (useQueryDocs.getState().docs.has(id)) unsaved.add(id);
        return false;
      },
    )
    .finally(() => {
      if (inflight.get(id) === p) inflight.delete(id);
    });
  inflight.set(id, p);
  return p;
}

function schedule(id: string) {
  const timer = timers.get(id);
  if (timer !== undefined) clearTimeout(timer);
  timers.set(
    id,
    setTimeout(() => void persist(id), AUTOSAVE_MS),
  );
}

/** Stops the autosave of a tab and waits for a save in flight (so it cannot recreate the row). */
async function forget(id: string) {
  const timer = timers.get(id);
  if (timer !== undefined) clearTimeout(timer);
  timers.delete(id);
  unsaved.delete(id);
  await inflight.get(id);
}

/** Shows `TxCloseDialog`; true once the transaction was committed or rolled back. */
function settleTransaction(id: string, title: string): Promise<boolean> {
  const connectionId = useQueryDocs.getState().docs.get(id)?.connectionId;
  const summary = connectionId ? useConnections.getState().summaries.get(connectionId) : undefined;
  return new Promise((resolve) => {
    useDialogs.getState().ask({
      kind: 'txClose',
      title,
      connectionName: summary?.config.name,
      envTag: summary?.config.env_tag ?? null,
      settle: async (action: TxAction) => {
        await queryTxRecorded(id, action);
      },
      done: (closed) => {
        useDialogs.getState().answer();
        resolve(closed);
      },
    });
  });
}

function confirmDiscard(file: string): Promise<boolean> {
  return new Promise((resolve) => {
    useDialogs.getState().ask({
      kind: 'discard',
      file,
      done: (discard) => {
        useDialogs.getState().answer();
        resolve(discard);
      },
    });
  });
}

/** The failures of tab I/O: an error toast that stays until dismissed. */
const notice = (text: string) => useDialogs.getState().setNotice(text, 'error');

export const useQueryDocs = create<QueryDocsState>()((set, get) => {
  /** Adds a document and its tab. */
  const add = (fields: Partial<QueryDoc> & Pick<QueryDoc, 'connectionId' | 'database'>): string => {
    const { docs } = get();
    const id = crypto.randomUUID();
    const doc: QueryDoc = {
      id,
      title: nextTitle(docs.values()),
      sql: '',
      cursor: 0,
      filePath: null,
      dirty: false,
      continueOnError: false,
      rowLimit: DEFAULT_ROW_LIMIT,
      sortOrder: Math.max(-1, ...[...docs.values()].map((d) => d.sortOrder)) + 1,
      dialect: dialectOfConnection(fields.connectionId),
      ...fields,
    };
    set({ docs: new Map(docs).set(id, doc) });
    useTabs.getState().open({ id, kind: 'query', connectionId: doc.connectionId, title: doc.title });
    schedule(id);
    return id;
  };

  return {
    docs: new Map(),
    newQuery: (connectionId, database) => add({ connectionId, database }),

    update: (id, patch) => {
      const doc = get().docs.get(id);
      if (!doc) return;
      const next = { ...doc, ...patch };
      if (next.filePath === null) next.dirty = false;
      else if (patch.sql !== undefined && patch.sql !== doc.sql && patch.dirty === undefined) next.dirty = true;
      set((s) => ({ docs: new Map(s.docs).set(id, next) }));
      if (next.title !== doc.title) useTabs.getState().rename(id, next.title);
      schedule(id);
    },

    reopen: (doc) => {
      if (get().docs.has(doc.id)) {
        useTabs.getState().activate(doc.id);
        return doc.id;
      }
      // The connection may have been deleted since: the tab comes back unbound.
      const stale = doc.connectionId !== null && !useConnections.getState().summaries.has(doc.connectionId);
      const next: QueryDoc = {
        ...doc,
        ...(stale ? { connectionId: null, database: null, dialect: dialectOfConnection(null) } : {}),
        sortOrder: Math.max(-1, ...[...get().docs.values()].map((d) => d.sortOrder)) + 1,
      };
      set((s) => ({ docs: new Map(s.docs).set(next.id, next) }));
      useTabs.getState().open({ id: next.id, kind: 'query', connectionId: next.connectionId, title: next.title });
      schedule(next.id);
      return next.id;
    },

    remove: (id) => {
      const timer = timers.get(id);
      if (timer !== undefined) clearTimeout(timer);
      timers.delete(id);
      set((s) => {
        if (!s.docs.has(id)) return s;
        const docs = new Map(s.docs);
        docs.delete(id);
        return { docs };
      });
    },

    restore: async ({ connectionsKnown = true } = {}) => {
      let records: QueryTabRecord[];
      try {
        records = await api.tabsLoad();
      } catch (e) {
        notice(`Could not restore the query tabs: ${toAppError(e).message}`);
        return;
      }
      // Without the stored connections (the tree failed to load) a binding cannot be checked: keep it.
      const known = connectionsKnown ? useConnections.getState().summaries : null;
      const fresh = [...records]
        .sort((a, b) => a.sort_order - b.sort_order)
        .filter((r) => !get().docs.has(r.id))
        .map((r) => {
          const doc = fromRecord(r);
          if (!doc.connectionId || !known || known.has(doc.connectionId)) return doc;
          // The connection was deleted: the tab stays, unbound ("Choose a connection").
          return { ...doc, connectionId: null, database: null, dialect: dialectOfConnection(null) };
        });
      if (!fresh.length) return;
      const docs = new Map(get().docs);
      for (const d of fresh) docs.set(d.id, d);
      set({ docs });
      const tabs = useTabs.getState();
      const hadActive = tabs.activeId;
      for (const d of fresh) tabs.open({ id: d.id, kind: 'query', connectionId: d.connectionId, title: d.title });
      useTabs.getState().activate(hadActive ?? fresh[0]!.id);
    },

    flush: async () => {
      const ids = new Set([...timers.keys(), ...unsaved, ...inflight.keys()]);
      // `persist` chains behind a save in flight, so this also waits for those.
      const results = await Promise.all([...ids].map(async (id) => [id, await persist(id)] as const));
      return results.filter(([, ok]) => !ok).map(([id]) => id);
    },

    closeTab: async (id) => {
      const tab = useTabs.getState().tabs.find((t) => t.id === id);
      if (tab && tab.kind !== 'query') {
        useTabs.getState().close(id);
        return true;
      }
      if (closing.has(id)) return false;
      closing.add(id);
      try {
        const title = get().docs.get(id)?.title ?? tab?.title ?? 'Query';
        // Ask, then stop the run and wait for it to end, so a transaction it holds can be settled.
        if (runningOf(id)) {
          if (!(await confirmStopRun(title))) return false;
          if (!(await stopRunOf(id))) {
            notice('Could not stop the query; the tab stays open.');
            return false;
          }
        }
        // Discard first: cancelling it must not leave a transaction committed or rolled back.
        const doc = get().docs.get(id);
        if (doc?.filePath && doc.dirty && !(await confirmDiscard(fileName(doc.filePath)))) return false;
        let settled = false;
        if (txStateOf(id) !== 'idle') {
          if (!(await settleTransaction(id, title))) return false;
          settled = true;
        }
        for (;;) {
          try {
            // After Commit / Rollback the session may still report a transaction (MySQL
            // `autocommit = 0`): the user has decided, so close anyway.
            await api.queryCloseTab(id, settled);
            break;
          } catch (e) {
            const err = toAppError(e);
            if (err.kind === 'transaction_open' && !settled) {
              if (!(await settleTransaction(id, title))) return false;
              settled = true;
              continue;
            }
            notice(`Could not close ${title}: ${err.message}`);
            return false;
          }
        }
        await forget(id);
        try {
          await api.tabsDelete(id);
        } catch {
          // The row stays and the tab comes back on the next start; nothing else to do.
        }
        // The text as it is now, for "Reopen closed tab".
        const snapshot = get().docs.get(id);
        useTabs.getState().close(id);
        if (snapshot) useTabs.getState().pushClosed({ kind: 'query', doc: snapshot });
        get().remove(id);
        return true;
      } finally {
        closing.delete(id);
      }
    },

    openFile: async () => {
      // The backend opens the dialog and only then lets `file_read` / `file_write` use the path.
      let picked: string | null;
      try {
        picked = await api.filePickOpen('sql');
      } catch (e) {
        notice(`Could not open a file: ${toAppError(e).message}`);
        return;
      }
      if (!picked) return;
      const already = [...get().docs.values()].find((d) => d.filePath === picked);
      if (already) {
        useTabs.getState().activate(already.id);
        return;
      }
      let text: string;
      try {
        text = await api.fileRead(picked);
      } catch (e) {
        notice(`Could not open ${fileName(picked)}: ${toAppError(e).message}`);
        return;
      }
      // The new tab takes the connection of the active query tab, if any.
      const active = useTabs.getState().activeId;
      const from = active ? get().docs.get(active) : undefined;
      add({
        connectionId: from?.connectionId ?? null,
        database: from?.database ?? null,
        title: fileName(picked),
        sql: text,
        filePath: picked,
        dirty: false,
      });
    },

    saveFile: async (id, saveAs = false) => {
      const doc = get().docs.get(id);
      if (!doc) return;
      let path = saveAs ? null : doc.filePath;
      if (!path) {
        try {
          path = await api.filePickSave('sql', null);
        } catch (e) {
          notice(`Could not save ${doc.title}: ${toAppError(e).message}`);
          return;
        }
        if (!path) return;
      }
      const text = doc.sql;
      try {
        await api.fileWrite(path, text);
      } catch (e) {
        notice(`Could not save ${fileName(path)}: ${toAppError(e).message}`);
        return;
      }
      // Typing during the write keeps the tab dirty.
      const now = get().docs.get(id);
      if (!now) return;
      get().update(id, { filePath: path, title: fileName(path), dirty: now.sql !== text });
    },

    reset: () => {
      for (const t of timers.values()) clearTimeout(t);
      timers.clear();
      inflight.clear();
      unsaved.clear();
      closing.clear();
      set({ docs: new Map() });
    },
  };
});

/** The part of the Tauri window the close guard uses. */
export interface CloseGuardWindow {
  onCloseRequested(handler: (event: { preventDefault(): void }) => void | Promise<void>): Promise<() => void>;
  destroy(): Promise<void>;
}

function confirmQuitUnsaved(count: number): Promise<boolean> {
  return new Promise((resolve) => {
    useDialogs.getState().ask({
      kind: 'saveFailed',
      count,
      done: (quit) => {
        useDialogs.getState().answer();
        resolve(quit);
      },
    });
  });
}

/**
 * The tabs to list before quitting: those with a run in progress (listed as running, and never
 * probed: `query_close_tab` stops a run before closing, which would cancel it before the user
 * decides), those the tx source reports non-idle, plus every idle tab whose session answers the
 * probe `query_close_tab(id, false)` with `transaction_open`, or `busy` (listed as running). The
 * probe stays as a backstop even with a source registered. It closes idle sessions on the way;
 * if the quit is cancelled they reopen on the next plan.
 *
 * Why ask at all, when both engines roll back on disconnect: a statement still running
 * holds its locks until it finishes on the server, and XA-prepared transactions survive
 * the disconnect.
 */
async function openTransactions(): Promise<{ doc: QueryDoc; running: boolean }[]> {
  const docs = [...useQueryDocs.getState().docs.values()].sort((a, b) => a.sortOrder - b.sortOrder);
  const probed = await Promise.all(
    docs.map(async (doc) => {
      if (runningOf(doc.id)) return { doc, open: true, running: true };
      try {
        await api.queryCloseTab(doc.id, false);
        return { doc, open: txStateOf(doc.id) !== 'idle', running: false };
      } catch (e) {
        const kind = toAppError(e).kind;
        return {
          doc,
          open: kind === 'transaction_open' || kind === 'busy' || txStateOf(doc.id) !== 'idle',
          running: kind === 'busy',
        };
      }
    }),
  );
  return probed.filter((p) => p.open).map(({ doc, running }) => ({ doc, running }));
}

/**
 * App close (spec §7.2): saves every tab (a failed save asks "Quit anyway / Cancel"), then
 * quits, unless tabs have an open transaction or a running statement: `AppCloseDialog` lists
 * them with "Rollback all & quit" / "Cancel".
 */
export function installCloseGuard(win: CloseGuardWindow): Promise<() => void> {
  let busy = false;
  return win.onCloseRequested(async (event) => {
    event.preventDefault();
    const asking = useDialogs.getState().question?.kind;
    if (busy || asking === 'appClose' || asking === 'saveFailed' || asking === 'discardEdits') return;
    busy = true;
    try {
      // Staged data edits are not saved anywhere: quitting throws them away, so ask first.
      if (!(await stagedEdits.confirm())) return;
      const failed = await useQueryDocs.getState().flush();
      if (failed.length && !(await confirmQuitUnsaved(failed.length))) return;
      const open = await openTransactions();
      if (!open.length) {
        await win.destroy();
        return;
      }
      useDialogs.getState().ask({
        kind: 'appClose',
        tabs: open.map(({ doc, running }) => (running ? `${doc.title} (running)` : doc.title)),
        quit: async () => {
          // Running tabs are stopped first (bounded wait), so their transaction can be rolled back.
          // A run that does not stop, or a failed rollback, does not hold the app: the server
          // rolls back when the session drops.
          await Promise.allSettled(open.filter((t) => runningOf(t.doc.id)).map(({ doc }) => stopRunOf(doc.id)));
          await Promise.allSettled(open.map(({ doc }) => api.queryTx(doc.id, 'Rollback')));
          await win.destroy();
        },
        done: () => useDialogs.getState().answer(),
      });
    } finally {
      busy = false;
    }
  });
}
