import Editor, { type OnMount } from '@monaco-editor/react';
import { useQueryDocs, type QueryDoc } from '../../store/queryTabs';
import { useTheme } from '../../theme/ThemeProvider';

export interface SqlEditorProps {
  doc: QueryDoc;
  engine: 'mysql' | 'postgres';
  onMount?: (editor: Parameters<OnMount>[0], monaco: Parameters<OnMount>[1]) => void;
}

/** The Monaco editor of a query document. Monaco itself is bundled by `monaco/setup.ts`. */
export function SqlEditor({ doc, engine, onMount }: SqlEditorProps) {
  const { dark } = useTheme();
  return (
    <div className="sql-editor" data-testid="query-editor">
      <Editor
        path={`query://${doc.id}`}
        language={engine === 'postgres' ? 'pgsql' : 'mysql'}
        value={doc.sql}
        theme={dark ? 'vs-dark' : 'vs'}
        onChange={(sql) => useQueryDocs.getState().update(doc.id, { sql: sql ?? '', dirty: true })}
        onMount={(editor, monaco) => onMount?.(editor, monaco)}
        options={{
          automaticLayout: true,
          minimap: { enabled: false },
          fontFamily: "ui-monospace, 'Cascadia Code', 'SF Mono', Menlo, Consolas, monospace",
          fontSize: 13,
          scrollBeyondLastLine: false,
          renderWhitespace: 'none',
        }}
      />
    </div>
  );
}
