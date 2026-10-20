// Monaco `CompletionItemProvider` for the query editor (spec §7.3), registered once for
// both SQL languages; per model it asks `ctxOf` which tab, connection and database it serves.
import type * as Monaco from 'monaco-editor';
import { commands } from '../../bindings';
import { useCompletion } from '../../store/completion';
import { useConnections } from '../../store/connections';
import { useQueryDocs } from '../../store/queryTabs';
import { suggest, type Suggestion } from './provider';

type Engine = 'mysql' | 'postgres';

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
    return await commands.statementRanges(text, engine === 'postgres' ? { engine: 'postgres' } : { engine: 'mysql' });
  } catch {
    // No ranges: `analyze` still cuts the statement at `;`.
    return [];
  }
}

export function registerCompletion(
  monaco: typeof Monaco,
  ctxOf: (model: unknown) => CompletionTarget | null,
): { dispose(): void } {
  const provider: Monaco.languages.CompletionItemProvider = {
    triggerCharacters: ['.'],
    async provideCompletionItems(model, position) {
      const target = ctxOf(model);
      if (!target) return { suggestions: [] };
      const { connId, db, engine } = target;
      const text = model.getValue();
      const offset = model.getOffsetAt(position);
      const [stmtRanges, scope, containers] = await Promise.all([
        ranges(text, engine),
        connId && db ? useCompletion.scope(connId, db) : Promise.resolve(null),
        connId && (db || engine === 'mysql') ? useCompletion.containers(connId, db ?? '', engine) : Promise.resolve([]),
      ]);
      const items = await suggest({
        text,
        offset,
        ranges: stmtRanges,
        engine,
        scope,
        containers,
        loadObjects: (q) => (connId ? useCompletion.objects(connId, db, engine, q) : Promise.resolve(null)),
      });
      const word = model.getWordUntilPosition(position);
      const range = new monaco.Range(position.lineNumber, word.startColumn, position.lineNumber, word.endColumn);
      return {
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

let registered: { dispose(): void } | null = null;

/** Registers the provider once for the app's lifetime (every editor shares it). */
export function ensureCompletion(monaco: typeof Monaco): void {
  registered ??= registerCompletion(monaco, queryDocTarget);
}
