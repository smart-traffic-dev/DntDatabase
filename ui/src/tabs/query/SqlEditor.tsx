import Editor, { type OnMount } from '@monaco-editor/react';
import { ensureCompletion } from '../../query/completion/monacoProvider';
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

/** The Monaco editor of a query document. Monaco itself is bundled by `monaco/setup.ts`. */
export function SqlEditor({ doc, engine, onMount }: SqlEditorProps) {
  return (
    <div className="sql-editor" data-testid="query-editor">
      <Editor
        path={`query://${doc.id}`}
        language={engine === 'postgres' ? 'pgsql' : 'mysql'}
        value={doc.sql}
        onChange={(sql) => useQueryDocs.getState().update(doc.id, { sql: sql ?? '', dirty: true })}
        onMount={(editor, monaco) => {
          ensureCompletion(monaco);
          bindRunKeys(doc.id, editor, monaco);
          onMount?.(editor, monaco);
        }}
        options={{
          automaticLayout: true,
          minimap: { enabled: false },
          fontFamily: "'JetBrains Mono', ui-monospace, monospace",
          fontSize: 13,
          scrollBeyondLastLine: false,
          renderWhitespace: 'none',
        }}
      />
    </div>
  );
}
