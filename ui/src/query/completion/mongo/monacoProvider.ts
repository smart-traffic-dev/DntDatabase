// Monaco `CompletionItemProvider` of the MongoDB console (`mongo` language): registered once
// per Monaco instance, it serves the query tabs whose connection is MongoDB.
import type * as Monaco from 'monaco-editor';
import { hasMongoCompletion, MONGO_LANGUAGE } from '../../../engine';
import { useCompletion } from '../../../store/completion';
import { useConnections } from '../../../store/connections';
import { useQueryDocs, type QueryDoc } from '../../../store/queryTabs';
import { openQuietly, quietOpenFailed } from '../../../store/reopen';
import { useTree } from '../../../store/tree';
import { analyzeMongo, type MongoAnalysis } from './context';
import { suggestMongo, wantsFields, type MongoData, type MongoSuggestion } from './suggest';

export interface MongoTarget {
  tabId: string;
  connId: string | null;
  /** The tab's database (what `db` is). */
  db: string | null;
}

/** The database `db` names in a tab: its own, else the connection's default (as the toolbar shows it). */
function tabDb(connId: string | null, db: string | null): string | null {
  return db ?? (connId ? useConnections.getState().summaries.get(connId)?.config.default_db : null) ?? null;
}

/** The completion target of a query document when its connection is MongoDB, else null. */
export function mongoTabTarget(doc: QueryDoc): MongoTarget | null {
  const summary = doc.connectionId ? useConnections.getState().summaries.get(doc.connectionId) : undefined;
  if (!hasMongoCompletion(summary?.config.kind ?? doc.dialect)) return null;
  return { tabId: doc.id, connId: doc.connectionId, db: tabDb(doc.connectionId, doc.database) };
}

/** The MongoDB query document behind a `query://<tab id>` model; null for any other tab. */
export function mongoDocTarget(model: unknown): MongoTarget | null {
  const uri = (model as { uri?: { scheme?: string; authority?: string } } | null)?.uri;
  if (uri?.scheme !== 'query' || !uri.authority) return null;
  const doc = useQueryDocs.getState().docs.get(uri.authority);
  return doc ? mongoTabTarget(doc) : null;
}

/** How long a suggestion waits for a load (collections, fields) before it answers with what it has. */
const LOAD_WAIT_MS = 1500;

const showSystem = () => useTree.getState().showSystem;

/**
 * The data `a` needs, from the completion cache: what is not loaded yet is started and its
 * load returned in `pending` (the caller decides whether to wait).
 */
function gather(target: MongoTarget, a: MongoAnalysis): { data: MongoData; pending: Promise<unknown>[] } {
  const data: MongoData = { objects: [], fields: [], databases: [] };
  const pending: Promise<unknown>[] = [];
  const { connId } = target;
  const c = a.context;
  const needsObjects = c.kind === 'db' || c.kind === 'collection' || wantsFields(a);
  if (!connId || (!needsObjects && c.kind !== 'use')) return { data, pending };
  // A restored tab: its connection is not open yet. Typing here opens it quietly.
  if (!useConnections.getState().info.has(connId) && !quietOpenFailed(connId)) {
    pending.push(openQuietly(connId));
    return { data, pending };
  }
  const system = showSystem();
  if (c.kind === 'use') {
    const dbs = useCompletion.peekMongoDatabases(connId, system);
    if (dbs === undefined) pending.push(useCompletion.mongoDatabases(connId, system));
    data.databases = dbs ?? [];
    return { data, pending };
  }
  const db = (c.kind === 'db' || c.kind === 'collection' || c.kind === 'key' ? c.db : null) ?? target.db;
  if (!db) return { data, pending };
  const objects = useCompletion.peekMongoObjects(connId, db, system);
  if (objects === undefined) {
    pending.push(useCompletion.mongoObjects(connId, db, system));
    return { data, pending };
  }
  data.objects = objects ?? [];
  if (c.kind === 'key' && c.coll !== null && wantsFields(a)) {
    // Only a collection the database lists: its kind addresses the detail.
    const obj = data.objects.find((o) => o.name === c.coll);
    if (obj) {
      const fields = useCompletion.peekMongoFields(connId, db, obj);
      if (fields === undefined) pending.push(useCompletion.mongoFields(connId, db, obj));
      data.fields = fields ?? [];
    }
  }
  return { data, pending };
}

function itemKind(monaco: typeof Monaco, kind: MongoSuggestion['kind']): Monaco.languages.CompletionItemKind {
  const K = monaco.languages.CompletionItemKind;
  switch (kind) {
    case 'keyword':
      return K.Keyword;
    case 'database':
      return K.Module;
    case 'collection':
      return K.Struct;
    case 'view':
      return K.Interface;
    case 'method':
      return K.Method;
    case 'field':
      return K.Field;
    case 'operator':
    case 'stage':
      return K.Operator;
    case 'helper':
      return K.Function;
    default:
      return K.Text;
  }
}

const sortText = (i: number) => String(i).padStart(5, '0');

/** Registers the provider for the `mongo` language; `ctxOf` says which tab a model belongs to. */
export function registerMongoCompletion(monaco: typeof Monaco, ctxOf: (model: unknown) => MongoTarget | null): { dispose(): void } {
  const provider: Monaco.languages.CompletionItemProvider = {
    triggerCharacters: ['.', '$', '{', ' '],
    provideCompletionItems(model, position, context, token) {
      const target = ctxOf(model);
      if (!target || token?.isCancellationRequested) return { suggestions: [] };
      const a = analyzeMongo(model.getValue(), model.getOffsetAt(position));
      const kind = a.context.kind;
      if (kind === 'none') return { suggestions: [] };
      // A space opens the list only where a name follows it (`use `, `show `, `{ `).
      const space = context?.triggerCharacter === ' ';
      if (space && kind !== 'use' && kind !== 'show' && kind !== 'key') return { suggestions: [] };
      const start = model.getPositionAt(a.from);
      const range = new monaco.Range(position.lineNumber, start.column, position.lineNumber, position.column);
      const snippetRule = monaco.languages.CompletionItemInsertTextRule?.InsertAsSnippet ?? 4;
      const toList = (list: MongoSuggestion[], incomplete: boolean): Monaco.languages.CompletionList => ({
        incomplete,
        suggestions: list.map((s, i) => ({
          label: s.label,
          kind: itemKind(monaco, s.kind),
          detail: s.detail,
          insertText: s.insertText,
          ...(s.snippet ? { insertTextRules: snippetRule } : {}),
          filterText: s.label,
          sortText: sortText(i),
          range,
        })),
      });
      let round = gather(target, a);
      if (round.pending.length === 0) return toList(suggestMongo(a, round.data), false);
      // Something is loading: wait for it a short while (Monaco shows "Loading…"), then
      // answer with what is there; `incomplete` makes the next keystroke ask again.
      return (async () => {
        const deadline = Date.now() + LOAD_WAIT_MS;
        // A cancelled request (the next keystroke) stops waiting at once: no timer outlives it.
        let cancel: (() => void) | undefined;
        const cancelled = new Promise<void>((r) => (cancel = r));
        const sub = token?.onCancellationRequested?.(() => cancel?.());
        try {
          // Opening the connection, the collections, then the fields: at most three loads in a row.
          for (let i = 0; i < 3 && round.pending.length > 0; i++) {
            const left = deadline - Date.now();
            if (left <= 0) break;
            let timer: ReturnType<typeof setTimeout> | undefined;
            await Promise.race([Promise.all(round.pending), cancelled, new Promise((r) => (timer = setTimeout(r, left)))]);
            clearTimeout(timer);
            if (token?.isCancellationRequested) return { suggestions: [] };
            round = gather(target, a);
          }
          return toList(suggestMongo(a, round.data), round.pending.length > 0);
        } finally {
          sub?.dispose();
        }
      })();
    },
  };
  const sub = monaco.languages.registerCompletionItemProvider(MONGO_LANGUAGE, provider);
  return { dispose: () => sub.dispose() };
}

/** Starts loading the collections of a MongoDB tab's database (when its editor opens). Never waits. */
export function prefetchMongoCompletion(connId: string | null, database: string | null): void {
  const db = tabDb(connId, database);
  if (connId && db) useCompletion.peekMongoObjects(connId, db, showSystem());
}

type Handle = { dispose(): void };

/** One registration per monaco instance (a WeakMap: the real monaco namespace is not extensible). */
const handles = new WeakMap<object, Handle>();

/** Registers the MongoDB console provider once per monaco instance. */
export function ensureMongoCompletion(monaco: typeof Monaco): void {
  if (handles.has(monaco)) return;
  handles.set(monaco, registerMongoCompletion(monaco, mongoDocTarget));
  if (import.meta.hot) import.meta.hot.data.mongoCompletion = { monaco, handle: handles.get(monaco) };
}

// HMR: the old module disposes its provider, the new one registers on the same monaco.
if (import.meta.hot) {
  const hot = import.meta.hot;
  const previous = hot.data.mongoCompletion as { monaco: typeof Monaco; handle: Handle } | undefined;
  if (previous) {
    hot.data.mongoCompletion = undefined;
    ensureMongoCompletion(previous.monaco);
  }
  hot.dispose((data: { mongoCompletion?: { monaco: typeof Monaco; handle: Handle } }) => {
    data.mongoCompletion?.handle.dispose();
  });
}
