import Editor, { type OnMount } from '@monaco-editor/react';
import type * as Monaco from 'monaco-editor';
import { memo, useEffect } from 'react';
import { useDntMonacoTheme } from '../../monaco/dntTheme';
import { bindEditorFontScale, editorFontOptions } from '../../monaco/fonts';
import './toolbar.css';
import { ensureCompletion, prefetchCompletion } from '../../query/completion/monacoProvider';
import type { Engine } from '../../query/engine';
import { clearCursor, reportCursor } from '../../store/cursor';
import { exportRunOf, useExporting } from '../../store/queryExport';
import { canStop, emptyRun, useQueryRun } from '../../store/queryRun';
import { explainQuery, registerEditor, runFromEditor, setHasSelection, stopQuery, unregisterEditor } from '../../store/runQuery';
import { useQueryDocs, type QueryDoc } from '../../store/queryTabs';
import { editorOf, setActiveEditor } from './activeEditor';
import { registerEditorCommands } from './editorCommands';

export interface SqlEditorProps {
  doc: QueryDoc;
  engine: Engine;
  onMount?: (editor: Parameters<OnMount>[0], monaco: Parameters<OnMount>[1]) => void;
}

type Editor = Parameters<OnMount>[0];
type Monaco = Parameters<OnMount>[1];

/** Ctrl+Enter / Ctrl+Shift+Enter run, Ctrl+Shift+E explains, Esc stops while running (spec §5.2). Not S/O/T: those are window shortcuts. */
export function bindRunKeys(tabId: string, editor: Editor, monaco: Monaco) {
  registerEditor(tabId, editor);
  setActiveEditor(tabId, editor);
  editor.onDidDispose(() => {
    unregisterEditor(tabId, editor);
    if (editorOf(tabId) === editor) setActiveEditor(tabId, null);
    setHasSelection(tabId, false);
  });
  editor.onDidChangeCursorSelection((e) => setHasSelection(tabId, !e.selection.isEmpty()));
  // Ln / Col / selected characters for the status bar: coalesced per frame in the cursor store.
  const reportPosition = () => {
    const p = editor.getPosition();
    if (!p) return;
    const model = editor.getModel();
    const selected = model ? (editor.getSelections() ?? []).reduce((n, s) => n + model.getValueLengthInRange(s), 0) : 0;
    reportCursor(tabId, { line: p.lineNumber, col: p.column, selected });
  };
  editor.onDidChangeCursorSelection(reportPosition);
  reportPosition();
  editor.onDidDispose(() => clearCursor(tabId));
  // Actions, not `addCommand`: a command lands in Monaco's one shared keybinding service and the
  // last mounted editor's handler wins in every editor, so with two query tabs side by side
  // Ctrl+Enter would run the other tab. An action is scoped to its editor and unbound on dispose.
  const bind = (id: string, label: string, key: number, run: () => void, precondition?: string) => {
    const handle = editor.addAction({ id: `dnt.run.${id}`, label, keybindings: [key], precondition, run });
    editor.onDidDispose(() => handle.dispose());
  };
  bind('cursor', 'Run statement', monaco.KeyMod.CtrlCmd | monaco.KeyCode.Enter, () => void runFromEditor(tabId, 'cursor'));
  bind('all', 'Run all', monaco.KeyMod.CtrlCmd | monaco.KeyMod.Shift | monaco.KeyCode.Enter, () => void runFromEditor(tabId, 'all'));
  bind('explain', 'Explain', monaco.KeyMod.CtrlCmd | monaco.KeyMod.Shift | monaco.KeyCode.KeyE, () => void explainQuery(tabId, false));
  // Esc keeps its editor meaning (close widgets, collapse cursors) unless a run can be stopped.
  // An export of the tab's result (it runs on the tab's session) is stoppable too.
  const stoppable = () => canStop(useQueryRun.getState().runs.get(tabId) ?? emptyRun) || exportRunOf(tabId) !== null;
  const running = editor.createContextKey<boolean>('dntRunning', stoppable());
  const unsubscribe = useQueryRun.subscribe(() => running.set(stoppable()));
  const unsubscribeExport = useExporting.subscribe(() => running.set(stoppable()));
  editor.onDidDispose(unsubscribe);
  editor.onDidDispose(unsubscribeExport);
  bind('stop', 'Stop', monaco.KeyCode.Escape, () => void stopQuery(tabId), 'dntRunning');
  // F9 / Ctrl+R / Ctrl+Shift+R, Format SQL, case, duplicate, F12: Monaco actions (context menu).
  registerEditorCommands(tabId, editor, monaco);
}

/**
 * Editor options: one object for the app's lifetime (a new object per render makes
 * @monaco-editor/react call `updateOptions` on every keystroke). Fonts come from the tokens.
 */
let editorOptions: Monaco.editor.IStandaloneEditorConstructionOptions | null = null;
function options(): Monaco.editor.IStandaloneEditorConstructionOptions {
  editorOptions ??= {
    automaticLayout: true,
    minimap: { enabled: false },
    ...editorFontOptions(),
    scrollBeyondLastLine: false,
    renderWhitespace: 'none',
    // No lightbulb / quick-fix actions: the error hover stays a plain message.
    lightbulb: { enabled: 'off' as never },
    // Our provider has the keywords and the schema: Monaco's own scan of every word of the
    // model (on the editor worker, per keystroke) only competes with it and slows big scripts.
    wordBasedSuggestions: 'off',
    // Snippet-kind items come from our provider only (built-in and user snippets), ranked last.
    snippetSuggestions: 'bottom',
    suggest: { showWords: false, showSnippets: true, filterGraceful: true, localityBonus: false },
  };
  return editorOptions;
}

/** The mounted editor of each tab: what it holds right now. */
const editors = new Map<string, { getValue(): string }>();
/**
 * Per tab, the text the editor last held as far as React knows: what it typed (`onChange`)
 * or the `value` it was last rendered with (applied without an `onChange`). Used when no
 * editor instance is mounted yet.
 */
const shownText = new Map<string, string>();

/**
 * Re-render only when something the editor shows changed. Every keystroke updates the
 * document in the store; re-rendering the editor for its own text is wasted work. The text
 * is skipped only when it equals what the editor holds now: a text loaded from history or
 * a file always reaches it, even one equal to an earlier typed text.
 */
function sameEditor(prev: SqlEditorProps, next: SqlEditorProps): boolean {
  if (prev.engine !== next.engine || prev.doc.id !== next.doc.id) return false;
  if (prev.doc.connectionId !== next.doc.connectionId || prev.doc.database !== next.doc.database) return false;
  const id = next.doc.id;
  const holds = editors.get(id)?.getValue() ?? shownText.get(id);
  return next.doc.sql === holds;
}

/** The Monaco editor of a query document. Monaco itself is bundled by `monaco/setup.ts`. */
export const SqlEditor = memo(SqlEditorView, sameEditor);

function SqlEditorView({ doc, engine, onMount }: SqlEditorProps) {
  const monacoTheme = useDntMonacoTheme();
  const { connectionId, database } = doc;
  // Load the schema for autocomplete as soon as the editor opens, not on the first suggestion.
  useEffect(() => {
    prefetchCompletion({ tabId: doc.id, connId: connectionId, db: database, engine });
  }, [doc.id, connectionId, database, engine]);
  // This render hands `doc.sql` to the editor as its value (applied without an onChange).
  shownText.set(doc.id, doc.sql);
  useEffect(
    () => () => {
      shownText.delete(doc.id);
      editors.delete(doc.id);
    },
    [doc.id],
  );
  return (
    <div className="sql-editor" data-testid="query-editor">
      <Editor
        path={`query://${doc.id}`}
        language={engine === 'postgres' ? 'pgsql' : 'mysql'}
        value={doc.sql}
        theme={monacoTheme.theme}
        beforeMount={monacoTheme.beforeMount}
        onChange={(sql) => {
          shownText.set(doc.id, sql ?? '');
          useQueryDocs.getState().update(doc.id, { sql: sql ?? '', dirty: true });
        }}
        onMount={(editor, monaco) => {
          ensureCompletion(monaco);
          editors.set(doc.id, editor);
          editor.onDidDispose(() => {
            if (editors.get(doc.id) === editor) editors.delete(doc.id);
          });
          bindRunKeys(doc.id, editor, monaco);
          bindEditorFontScale(editor);
          onMount?.(editor, monaco);
        }}
        options={options()}
      />
    </div>
  );
}
