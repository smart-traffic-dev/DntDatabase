import { create } from 'zustand';
import { useConnections } from './connections';
import { useTabs } from './tabs';

export type RowLimit = 1000 | 10000 | 100000 | 1000000;
export const ROW_LIMITS: RowLimit[] = [1000, 10000, 100000, 1000000];
export const DEFAULT_ROW_LIMIT: RowLimit = 10000;

/** SQL dialect of a document: follows the engine of its connection. */
export type Dialect = 'mysql' | 'postgres';

/** The editor document of a query tab (persistence comes with Task 21). */
export interface QueryDoc {
  id: string;
  title: string;
  connectionId: string | null;
  database: string | null;
  sql: string;
  /** UTF-16 offset of the cursor. */
  cursor: number;
  filePath: string | null;
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
  update(id: string, patch: Partial<QueryDoc>): void;
  remove(id: string): void;
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

export const dialectOfConnection = (connectionId: string | null): Dialect =>
  connectionId && useConnections.getState().summaries.get(connectionId)?.config.kind === 'postgres'
    ? 'postgres'
    : 'mysql';

export const useQueryDocs = create<QueryDocsState>()((set, get) => ({
  docs: new Map(),
  newQuery: (connectionId, database) => {
    const { docs } = get();
    const id = crypto.randomUUID();
    const title = nextTitle(docs.values());
    const doc: QueryDoc = {
      id,
      title,
      connectionId,
      database,
      sql: '',
      cursor: 0,
      filePath: null,
      dirty: false,
      continueOnError: false,
      rowLimit: DEFAULT_ROW_LIMIT,
      sortOrder: Math.max(-1, ...[...docs.values()].map((d) => d.sortOrder)) + 1,
      dialect: dialectOfConnection(connectionId),
    };
    set({ docs: new Map(docs).set(id, doc) });
    useTabs.getState().open({ id, kind: 'query', connectionId, title });
    return id;
  },
  update: (id, patch) =>
    set((s) => {
      const doc = s.docs.get(id);
      return doc ? { docs: new Map(s.docs).set(id, { ...doc, ...patch }) } : s;
    }),
  remove: (id) =>
    set((s) => {
      if (!s.docs.has(id)) return s;
      const docs = new Map(s.docs);
      docs.delete(id);
      return { docs };
    }),
  reset: () => set({ docs: new Map() }),
}));
