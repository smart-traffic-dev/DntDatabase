import { open, save } from '@tauri-apps/plugin-dialog';
import { create } from 'zustand';
import { api, toAppError } from '../api';
import type { QueryTabRecord, TxAction, TxState } from '../bindings';
import { useConnections } from './connections';
import { useDialogs } from './dialogs';
import { useTabs } from './tabs';
import { useTree } from './tree';

export type RowLimit = 1000 | 10000 | 100000 | 1000000;
export const ROW_LIMITS: RowLimit[] = [1000, 10000, 100000, 1000000];
export const DEFAULT_ROW_LIMIT: RowLimit = 10000;

/** Debounce of the tab autosave (spec §7.2). */
export const AUTOSAVE_MS = 1000;

const SQL_FILTERS = [{ name: 'SQL', extensions: ['sql'] }];

/** SQL dialect of a document: follows the engine of its connection. */
export type Dialect = 'mysql' | 'postgres';

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
  dialect: Dialect;
}

interface QueryDocsState {
  docs: Map<string, QueryDoc>;
  /** Creates the document and its tab, returns the id. */
  newQuery(connectionId: string | null, database: string | null): string;
  /** Changes a document and schedules its autosave (`AUTOSAVE_MS`). */
  update(id: string, patch: Partial<QueryDoc>): void;
  /** Drops the document from memory only (no backend call). */
  remove(id: string): void;
  /** Opens the tabs saved by `tabs_save`. Never connects. */
  restore(): Promise<void>;
  /** Saves every tab whose autosave is pending, now. */
  flush(): Promise<void>;
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

export const dialectOfConnection = (connectionId: string | null): Dialect =>
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

export function setTxStateSource(source: ((tabId: string) => TxState) | null) {
  txStateOf = source ?? (() => 'idle');
}

const timers = new Map<string, ReturnType<typeof setTimeout>>();
const inflight = new Map<string, Promise<void>>();
const closing = new Set<string>();

/** Saves one tab now; failures are left to the next autosave (internal ones reach the toast). */
function persist(id: string): Promise<void> {
  const timer = timers.get(id);
  if (timer !== undefined) clearTimeout(timer);
  timers.delete(id);
  if (!useQueryDocs.getState().docs.has(id)) return Promise.resolve();
  const previous = inflight.get(id) ?? Promise.resolve();
  const p = previous
    .then(() => {
      // Saves the document as it is when its turn comes.
      const doc = useQueryDocs.getState().docs.get(id);
      return doc ? api.tabsSave(toRecord(doc)) : null;
    })
    .then(
      () => undefined,
      () => undefined,
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
  await inflight.get(id);
}

/** Shows `TxCloseDialog`; true once the transaction was committed or rolled back. */
function settleTransaction(id: string, title: string): Promise<boolean> {
  return new Promise((resolve) => {
    useDialogs.getState().ask({
      kind: 'txClose',
      title,
      settle: async (action: TxAction) => {
        await api.queryTx(id, action);
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

const notice = (text: string) => useDialogs.getState().setNotice(text);

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

    restore: async () => {
      let records: QueryTabRecord[];
      try {
        records = await api.tabsLoad();
      } catch (e) {
        notice(`Could not restore the query tabs: ${toAppError(e).message}`);
        return;
      }
      // Without the stored connections (the tree failed to load) a binding cannot be checked: keep it.
      const known = useTree.getState().error ? null : useConnections.getState().summaries;
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
      await Promise.all([...timers.keys()].map((id) => persist(id)));
      await Promise.all([...inflight.values()]);
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
        let settled = false;
        if (txStateOf(id) !== 'idle') {
          if (!(await settleTransaction(id, title))) return false;
          settled = true;
        }
        const doc = get().docs.get(id);
        if (doc?.filePath && doc.dirty && !(await confirmDiscard(fileName(doc.filePath)))) return false;
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
        useTabs.getState().close(id);
        get().remove(id);
        return true;
      } finally {
        closing.delete(id);
      }
    },

    openFile: async () => {
      const picked = await open({ multiple: false, directory: false, filters: SQL_FILTERS });
      if (typeof picked !== 'string') return;
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
        path = await save({ filters: SQL_FILTERS });
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

/**
 * App close (spec §7.2): saves every tab, then quits, unless tabs have an open
 * transaction: `AppCloseDialog` lists them with "Rollback all & quit" / "Cancel".
 */
export function installCloseGuard(win: CloseGuardWindow): Promise<() => void> {
  let busy = false;
  return win.onCloseRequested(async (event) => {
    event.preventDefault();
    if (busy || useDialogs.getState().question?.kind === 'appClose') return;
    busy = true;
    try {
      const docs = useQueryDocs.getState();
      await docs.flush();
      const open = [...docs.docs.values()]
        .filter((d) => txStateOf(d.id) !== 'idle')
        .sort((a, b) => a.sortOrder - b.sortOrder);
      if (!open.length) {
        await win.destroy();
        return;
      }
      useDialogs.getState().ask({
        kind: 'appClose',
        tabs: open.map((d) => d.title),
        quit: async () => {
          // A failed rollback does not hold the app: the server rolls back when the session drops.
          await Promise.allSettled(open.map((d) => api.queryTx(d.id, 'Rollback')));
          await win.destroy();
        },
        done: () => useDialogs.getState().answer(),
      });
    } finally {
      busy = false;
    }
  });
}
