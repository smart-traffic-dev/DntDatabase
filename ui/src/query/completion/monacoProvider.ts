// Monaco `CompletionItemProvider` for the query editor (spec §7.3), registered once for
// both SQL languages; per model it asks `ctxOf` which tab, connection and database it serves.
import type * as Monaco from 'monaco-editor';
import { commands } from '../../bindings';
import { useCompletion } from '../../store/completion';
import { useConnections } from '../../store/connections';
import { engineDialect, useQueryDocs } from '../../store/queryTabs';
import type { Engine } from '../engine';
import { localStatementRange } from './context';
import { suggest, type Suggestion } from './provider';

export interface CompletionTarget {
  tabId: string;
  connId: string | null;
  db: string | null;
  engine: Engine;
}

const LANGUAGES = ['mysql', 'pgsql'];

function itemKind(monaco: typeof Monaco, kind: Suggestion['kind']): Monaco.languages.CompletionItemKind {
  const K = monaco.languages.CompletionItemKind;
  switch (kind) {
    case 'keyword':
      return K.Keyword;
    case 'function':
      return K.Function;
    case 'table':
      return K.Struct;
    case 'view':
      return K.Interface;
    case 'column':
      return K.Field;
    default:
      return K.Module;
  }
}

async function ranges(text: string, engine: Engine): Promise<[number, number][]> {
  try {
    return await commands.statementRanges(text, engineDialect(engine));
  } catch {
    // No ranges: `analyze` still cuts the statement at `;`.
    return [];
  }
}

/** Above this many UTF-16 code units (~1 MB) the text is not sent to `statement_ranges`. */
const LARGE_TEXT = 1024 * 1024;

export function registerCompletion(
  monaco: typeof Monaco,
  ctxOf: (model: unknown) => CompletionTarget | null,
): { dispose(): void } {
  /** Statement ranges per model, reused while its version (and the engine) is unchanged. */
  const rangeCache = new WeakMap<object, { version: number; engine: Engine; ranges: Promise<[number, number][]> }>();
  const rangesOf = (model: Monaco.editor.ITextModel, text: string, engine: Engine) => {
    const version = model.getVersionId();
    const hit = rangeCache.get(model);
    if (hit && hit.version === version && hit.engine === engine) return hit.ranges;
    const entry = { version, engine, ranges: ranges(text, engine) };
    rangeCache.set(model, entry);
    return entry.ranges;
  };

  const provider: Monaco.languages.CompletionItemProvider = {
    triggerCharacters: ['.'],
    async provideCompletionItems(model, position, _context, token) {
      const target = ctxOf(model);
      if (!target) return { suggestions: [] };
      const { connId, db, engine } = target;
      const text = model.getValue();
      const offset = model.getOffsetAt(position);
      const [stmtRanges, scope, containers] = await Promise.all([
        text.length > LARGE_TEXT
          ? // No IPC round trip with the whole script: cut the statement locally.
            Promise.resolve([localStatementRange(text, offset, engine)])
          : rangesOf(model, text, engine),
        connId && db ? useCompletion.scope(connId, db) : Promise.resolve(null),
        connId && (db || engine === 'mysql') ? useCompletion.containers(connId, db ?? '', engine) : Promise.resolve([]),
      ]);
      if (token?.isCancellationRequested) return { suggestions: [] };
      const items = await suggest({
        text,
        offset,
        ranges: stmtRanges,
        engine,
        scope,
        containers,
        loadObjects: (q) => (connId ? useCompletion.objects(connId, db, engine, q) : Promise.resolve(null)),
      });
      if (token?.isCancellationRequested) return { suggestions: [] };
      const word = model.getWordUntilPosition(position);
      const range = new monaco.Range(position.lineNumber, word.startColumn, position.lineNumber, word.endColumn);
      return {
        // No scope (failed or not loaded): Monaco asks again on the next keystroke instead
        // of only filtering a list without the tables.
        incomplete: Boolean(connId && db) && scope === null,
        suggestions: items.map((s, i) => ({
          label: s.label,
          kind: itemKind(monaco, s.kind),
          detail: s.detail,
          insertText: s.insertText,
          // Keep the provider's order: columns, then keywords, and so on.
          sortText: String(i).padStart(5, '0'),
          range,
        })),
      };
    },
  };
  const subs = LANGUAGES.map((lang) => monaco.languages.registerCompletionItemProvider(lang, provider));
  return { dispose: () => subs.forEach((s) => s.dispose()) };
}

/** The query document behind a `query://<tab id>` model (see `SqlEditor`). */
export function queryDocTarget(model: unknown): CompletionTarget | null {
  const uri = (model as { uri?: { scheme?: string; authority?: string } } | null)?.uri;
  if (uri?.scheme !== 'query' || !uri.authority) return null;
  const doc = useQueryDocs.getState().docs.get(uri.authority);
  if (!doc) return null;
  const kind = doc.connectionId ? useConnections.getState().summaries.get(doc.connectionId)?.config.kind : undefined;
  return { tabId: doc.id, connId: doc.connectionId, db: doc.database, engine: kind ?? doc.dialect };
}

type Handle = { dispose(): void };

/**
 * One registration per monaco instance. A WeakMap, not a property: the real monaco is an
 * ES module namespace object (`import * as monaco`), which is not extensible.
 */
const handles = new WeakMap<object, Handle>();

/** Registers the provider once per monaco instance (every editor shares it). */
export function ensureCompletion(monaco: typeof Monaco): void {
  if (handles.has(monaco)) return;
  const handle = registerCompletion(monaco, queryDocTarget);
  handles.set(monaco, handle);
  if (import.meta.hot) {
    import.meta.hot.data.completion = { monaco, handle };
  }
}

/** Removes the provider registered by `ensureCompletion`. */
export function disposeCompletion(monaco: typeof Monaco): void {
  handles.get(monaco)?.dispose();
  handles.delete(monaco);
}

// HMR: the old module disposes its provider (its handle is kept in `hot.data`), and the
// new module registers its own on the same monaco instance.
if (import.meta.hot) {
  const hot = import.meta.hot;
  const previous = hot.data.completion as { monaco: typeof Monaco; handle: Handle } | undefined;
  if (previous) {
    hot.data.completion = undefined;
    ensureCompletion(previous.monaco);
  }
  hot.dispose((data: { completion?: { monaco: typeof Monaco; handle: Handle } }) => {
    data.completion?.handle.dispose();
  });
}
