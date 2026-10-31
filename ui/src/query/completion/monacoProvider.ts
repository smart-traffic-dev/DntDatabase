// Monaco `CompletionItemProvider` for the query editor (spec §7.3), registered once for
// both SQL languages; per model it asks `ctxOf` which tab, connection and database it serves.
import type * as Monaco from 'monaco-editor';
import { useCompletion } from '../../store/completion';
import { useConnections } from '../../store/connections';
import { useQueryDocs } from '../../store/queryTabs';
import { openQuietly, quietOpenFailed } from '../../store/reopen';
import { sqlEngine, type Engine } from '../engine';
import { suggest, type Suggestion } from './provider';
import { allSnippets } from './snippets';
import { splitIncremental, type SplitState } from './split';

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

/** `sortText` of the i-th item (keeps the provider's order), built once per index. */
const sortTexts: string[] = [];
const sortText = (i: number) => (sortTexts[i] ??= String(i).padStart(5, '0'));

/** `Snippet` item kind and `InsertAsSnippet` rule (27 and 4 in Monaco), tolerant of partial test doubles. */
function snippetItems(monaco: typeof Monaco, range: Monaco.IRange): Monaco.languages.CompletionItem[] {
  const kind = monaco.languages.CompletionItemKind?.Snippet ?? 27;
  const rules = monaco.languages.CompletionItemInsertTextRule?.InsertAsSnippet ?? 4;
  // 'zz' sorts after the 5-digit indexes of columns, tables, keywords and functions.
  return allSnippets().map((s, i) => ({
    label: s.label,
    kind,
    detail: s.description,
    documentation: { value: '```sql\n' + s.body.replace(/\$\{\d+:([^}]*)\}/g, '$1') + '\n```' },
    insertText: s.body,
    insertTextRules: rules,
    filterText: s.prefix,
    sortText: `zz${String(i).padStart(4, '0')}`,
    range,
  }));
}

/**
 * Whether the suggest list of `editor` is on screen. Monaco adds `visible` 100 ms after it
 * shows the list (a fade-in), so the class alone misses a load that settles in that window.
 */
function suggestShown(editor: Monaco.editor.ICodeEditor): boolean {
  const widget = editor.getDomNode()?.querySelector('.suggest-widget');
  if (!widget) return false;
  const check = (widget as Element & { checkVisibility?: (o: object) => boolean }).checkVisibility;
  return check ? check.call(widget, { visibilityProperty: true }) : widget.classList.contains('visible');
}

/** Starts loading the autocomplete data of a target (e.g. when its editor opens). Never waits. */
export function prefetchCompletion(target: CompletionTarget): void {
  const { connId, db, engine } = target;
  if (!connId) return;
  if (db) useCompletion.peekScope(connId, db);
  if (db || engine === 'mysql') useCompletion.peekContainers(connId, db ?? '', engine);
}

/**
 * Registers the provider. The hot path is synchronous: statement ranges come from the local
 * splitter (cached per model version, no `statement_ranges` IPC per keystroke) and the scope
 * from the completion cache. While the scope is still loading, keywords are returned at once
 * with `incomplete` so Monaco asks again once the tables are there.
 */
export function registerCompletion(
  monaco: typeof Monaco,
  ctxOf: (model: unknown) => CompletionTarget | null,
): { dispose(): void } {
  /**
   * Text and statement split per model, reused while its version is unchanged. `changedFrom`
   * is the smallest offset edited since the last split (tracked from content events), so the
   * next split only rescans from the statement boundary before it.
   */
  interface Snapshot {
    version: number;
    text: string;
    split: SplitState;
    changedFrom: number;
    listener?: { dispose(): void };
  }
  const snapshots = new WeakMap<object, Snapshot>();
  const snapshot = (model: Monaco.editor.ITextModel, engine: Engine): { text: string; ranges: [number, number][] } => {
    const version = model.getVersionId();
    let snap = snapshots.get(model);
    if (snap && snap.version === version && snap.split.engine === engine) return { text: snap.text, ranges: snap.split.ranges };
    const text = model.getValue();
    if (!snap) {
      snap = { version, text, split: splitIncremental(null, text, engine, 0), changedFrom: Infinity };
      const s = snap;
      // Without content events (tests' fake models) every new version is a full split.
      if (typeof model.onDidChangeContent === 'function') {
        s.listener = model.onDidChangeContent((e) => {
          for (const c of e.changes) s.changedFrom = Math.min(s.changedFrom, c.rangeOffset);
        });
        model.onWillDispose?.(() => s.listener?.dispose());
      } else s.changedFrom = 0;
      snapshots.set(model, s);
    } else {
      snap.split = splitIncremental(snap.split, text, engine, Number.isFinite(snap.changedFrom) ? snap.changedFrom : 0);
      snap.version = version;
      snap.text = text;
      snap.changedFrom = snap.listener ? Infinity : 0;
    }
    return { text: snap.text, ranges: snap.split.ranges };
  };

  /** The load each model's open suggest list waits for (one refresh per load, not per keystroke). */
  const waiting = new WeakMap<object, Promise<unknown>>();
  /**
   * Once `load` settles, asks again in the editors of `model` whose suggest list is still
   * open, so the tables appear without another keystroke. A closed list stays closed.
   */
  const refreshWhenLoaded = (model: Monaco.editor.ITextModel, load: Promise<unknown>) => {
    if (waiting.get(model) === load) return;
    waiting.set(model, load);
    void load.then(() => {
      if (waiting.get(model) !== load) return;
      waiting.delete(model);
      for (const editor of monaco.editor?.getEditors?.() ?? []) {
        if (editor.getModel() !== model) continue;
        if (!suggestShown(editor)) continue;
        editor.trigger('dnt', 'editor.action.triggerSuggest', {});
      }
    });
  };

  const provider: Monaco.languages.CompletionItemProvider = {
    triggerCharacters: ['.'],
    provideCompletionItems(model, position, _context, token) {
      const target = ctxOf(model);
      if (!target || token?.isCancellationRequested) return { suggestions: [] };
      const { connId, db, engine } = target;
      const { text, ranges } = snapshot(model, engine);
      const offset = model.getOffsetAt(position);
      // `undefined`: still loading (the load is started, never awaited here).
      const scope = connId && db ? useCompletion.peekScope(connId, db) : null;
      const containers =
        connId && (db || engine === 'mysql') ? useCompletion.peekContainers(connId, db ?? '', engine) : [];
      const items = suggest({
        text,
        offset,
        ranges,
        engine,
        scope: scope ?? null,
        containers: containers ?? [],
        loadObjects: (q) => (connId ? useCompletion.objectsNow(connId, db, engine, q) : null),
      });
      const word = model.getWordUntilPosition(position);
      const range = new monaco.Range(position.lineNumber, word.startColumn, position.lineNumber, word.endColumn);
      const loads = () =>
        Promise.all([
          db ? useCompletion.scope(connId as string, db) : null,
          db || engine === 'mysql' ? useCompletion.containers(connId as string, db ?? '', engine) : null,
        ]);
      // The scope failed because the connection is not open yet (a restored tab: restoring
      // never connects). Typing here opens it quietly, then the tables load.
      const opening =
        Boolean(connId && db) &&
        scope === null &&
        !useConnections.getState().info.has(connId as string) &&
        !quietOpenFailed(connId as string);
      if (opening) refreshWhenLoaded(model, openQuietly(connId as string).then((ok) => (ok ? loads() : null)));
      else if (connId && (scope === undefined || containers === undefined)) refreshWhenLoaded(model, loads());
      // Still loading (or opening): Monaco asks again on the next keystroke instead of only
      // filtering a list without the tables. A failed load is final until it is retried.
      const incomplete = (Boolean(connId && db) && scope === undefined) || containers === undefined || opening;
      // Snippets are not offered after a `.` (a member name follows).
      const dotted = text[offset - (word.endColumn - word.startColumn) - 1] === '.';
      const toList = (list: Suggestion[]): Monaco.languages.CompletionList => ({
        incomplete,
        suggestions: [
          ...list.map((s, i) => ({
            label: s.label,
            kind: itemKind(monaco, s.kind),
            detail: s.detail,
            insertText: s.insertText,
            sortText: sortText(i),
            range,
          })),
          ...(dotted ? [] : snippetItems(monaco, range)),
        ],
      });
      if (Array.isArray(items)) return toList(items);
      // Only the objects of a `db.` qualifier not loaded yet: the one case that waits.
      return items.then((list) => (token?.isCancellationRequested ? { suggestions: [] } : toList(list)));
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
  return { tabId: doc.id, connId: doc.connectionId, db: doc.database, engine: sqlEngine(kind) ?? doc.dialect };
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
