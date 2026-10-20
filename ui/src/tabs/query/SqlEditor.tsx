import Editor, { type OnMount } from '@monaco-editor/react';
import type * as Monaco from 'monaco-editor';
import { memo, useEffect, useState } from 'react';
import { useDntMonacoTheme } from '../../monaco/dntTheme';
import { editorFontOptions } from '../../monaco/fonts';
import './toolbar.css';
import { ensureCompletion, prefetchCompletion } from '../../query/completion/monacoProvider';
import type { Engine } from '../../query/engine';
import { canStop, emptyRun, useQueryRun } from '../../store/queryRun';
import { explainQuery, registerEditor, runFromEditor, setHasSelection, stopQuery, unregisterEditor } from '../../store/runQuery';
import { useQueryDocs, type QueryDoc } from '../../store/queryTabs';

export interface SqlEditorProps {
  doc: QueryDoc;
  engine: Engine;
  onMount?: (editor: Parameters<OnMount>[0], monaco: Parameters<OnMount>[1]) => void;
}

type Editor = Parameters<OnMount>[0];
type Monaco = Parameters<OnMount>[1];

/** Ctrl+Enter / Ctrl+Shift+Enter run, Ctrl+Shift+E explains, Esc stops while running (spec §5.2). Not S/O/T: those are window shortcuts. */
function bindRunKeys(tabId: string, editor: Editor, monaco: Monaco) {
  registerEditor(tabId, editor);
  editor.onDidDispose(() => {
    unregisterEditor(tabId, editor);
    setHasSelection(tabId, false);
  });
  editor.onDidChangeCursorSelection((e) => setHasSelection(tabId, !e.selection.isEmpty()));
  editor.addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyCode.Enter, () => void runFromEditor(tabId, 'cursor'));
  editor.addCommand(
    monaco.KeyMod.CtrlCmd | monaco.KeyMod.Shift | monaco.KeyCode.Enter,
    () => void runFromEditor(tabId, 'all'),
  );
  editor.addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyMod.Shift | monaco.KeyCode.KeyE, () => void explainQuery(tabId, false));
  // Esc keeps its editor meaning (close widgets, collapse cursors) unless a run can be stopped.
  const running = editor.createContextKey<boolean>('dntRunning', canStop(useQueryRun.getState().get(tabId)));
  const unsubscribe = useQueryRun.subscribe((s) => running.set(canStop(s.runs.get(tabId) ?? emptyRun)));
  editor.onDidDispose(unsubscribe);
  editor.addCommand(monaco.KeyCode.Escape, () => void stopQuery(tabId), 'dntRunning');
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
    snippetSuggestions: 'none',
    suggest: { showWords: false, showSnippets: false, filterGraceful: true, localityBonus: false },
  };
  return editorOptions;
}

/** The text most recently sent to the store by each editor (`onChange`), per tab. */
const emitted = new Map<string, string>();

/**
 * Re-render only when something the editor shows changed. Every keystroke updates the
 * document in the store; re-rendering the editor for its own text is wasted work.
 */
function sameEditor(prev: SqlEditorProps, next: SqlEditorProps): boolean {
  if (prev.engine !== next.engine || prev.doc.id !== next.doc.id) return false;
  if (prev.doc.connectionId !== next.doc.connectionId || prev.doc.database !== next.doc.database) return false;
  if ((prev.doc.sql === '') !== (next.doc.sql === '')) return false;
  return prev.doc.sql === next.doc.sql || next.doc.sql === emitted.get(next.doc.id);
}

/** The Monaco editor of a query document. Monaco itself is bundled by `monaco/setup.ts`. */
export const SqlEditor = memo(SqlEditorView, sameEditor);

function SqlEditorView({ doc, engine, onMount }: SqlEditorProps) {
  const monacoTheme = useDntMonacoTheme();
  // The placeholder lines up with the text: left edge of the content area, known once the editor is laid out.
  const [contentLeft, setContentLeft] = useState<number | null>(null);
  const { connectionId, database } = doc;
  // Load the schema for autocomplete as soon as the editor opens, not on the first suggestion.
  useEffect(() => {
    prefetchCompletion({ tabId: doc.id, connId: connectionId, db: database, engine });
  }, [doc.id, connectionId, database, engine]);
  useEffect(() => () => void emitted.delete(doc.id), [doc.id]);
  return (
    <div className="sql-editor" data-testid="query-editor">
      {doc.sql === '' && (
        <div
          className="sql-editor-placeholder"
          data-testid="editor-placeholder"
          aria-hidden="true"
          style={{ left: contentLeft ?? 0, visibility: contentLeft === null ? 'hidden' : 'visible' }}
        >
          Ctrl+Enter runs the statement at the cursor · Ctrl+Shift+Enter runs all · Ctrl+Shift+E explains
        </div>
      )}
      <Editor
        path={`query://${doc.id}`}
        language={engine === 'postgres' ? 'pgsql' : 'mysql'}
        value={doc.sql}
        theme={monacoTheme.theme}
        beforeMount={monacoTheme.beforeMount}
        onChange={(sql) => {
          emitted.set(doc.id, sql ?? '');
          useQueryDocs.getState().update(doc.id, { sql: sql ?? '', dirty: true });
        }}
        onMount={(editor, monaco) => {
          ensureCompletion(monaco);
          setContentLeft(editor.getLayoutInfo().contentLeft);
          editor.onDidLayoutChange((l) => setContentLeft(l.contentLeft));
          bindRunKeys(doc.id, editor, monaco);
          onMount?.(editor, monaco);
        }}
        options={options()}
      />
    </div>
  );
}
