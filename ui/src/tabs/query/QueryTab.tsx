import { memo, useCallback, useState } from 'react';
import { useShallow } from 'zustand/react/shallow';
import { useConnections } from '../../store/connections';
import { emptyRun, useQueryRun } from '../../store/queryRun';
import { useQueryDocs } from '../../store/queryTabs';
import type { QueryTabRef } from '../../store/tabs';
import type { Engine } from '../../query/engine';
import { useEditorMarks, type EditorLike, type MonacoLike } from './editorMarks';
import { QueryToolbar } from './QueryToolbar';
import { ResultPanel } from './ResultPanel';
import { SqlEditor, type SqlEditorProps } from './SqlEditor';
import './query.css';

/**
 * The editor and its document. The only part of a query tab that subscribes to the text:
 * a keystroke re-renders this, not the toolbar or the result panel.
 */
const EditorHost = memo(function EditorHost({
  tabId,
  engine,
  onMount,
}: {
  tabId: string;
  engine: Engine;
  onMount: SqlEditorProps['onMount'];
}) {
  const doc = useQueryDocs((s) => s.docs.get(tabId));
  return doc ? <SqlEditor doc={doc} engine={engine} onMount={onMount} /> : null;
});

/** A query tab: toolbar over the editor, results and messages below. */
export function QueryTab({ tab }: { tab: QueryTabRef }) {
  // Field selectors only (not the document): the text changes on every keystroke.
  const toolbarDoc = useQueryDocs(
    useShallow((s) => {
      const d = s.docs.get(tab.id);
      return d
        ? {
            id: d.id,
            connectionId: d.connectionId,
            database: d.database,
            rowLimit: d.rowLimit,
            continueOnError: d.continueOnError,
            dialect: d.dialect,
          }
        : null;
    }),
  );
  const connectionId = toolbarDoc?.connectionId ?? null;
  const kind = useConnections((s) => (connectionId ? s.summaries.get(connectionId)?.config.kind : undefined));
  const run = useQueryRun((s) => s.runs.get(tab.id) ?? emptyRun);
  const [mounted, setMounted] = useState<{ editor: EditorLike; monaco: MonacoLike } | null>(null);
  const onMount = useCallback<NonNullable<SqlEditorProps['onMount']>>((editor, monaco) => setMounted({ editor, monaco }), []);
  const engine = kind ?? toolbarDoc?.dialect ?? 'mysql';
  useEditorMarks(mounted, engine, run);
  if (!toolbarDoc) {
    return (
      <div className="empty">
        <strong>Query not found</strong>
      </div>
    );
  }
  return (
    <div className="query-tab">
      <QueryToolbar doc={toolbarDoc} />
      <EditorHost tabId={tab.id} engine={engine} onMount={onMount} />
      <ResultPanel tabId={tab.id} />
    </div>
  );
}
