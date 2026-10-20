import { useState } from 'react';
import { useConnections } from '../../store/connections';
import { emptyRun, useQueryRun } from '../../store/queryRun';
import { useQueryDocs } from '../../store/queryTabs';
import type { QueryTabRef } from '../../store/tabs';
import { useEditorMarks, type EditorLike, type MonacoLike } from './editorMarks';
import { QueryToolbar } from './QueryToolbar';
import { ResultPanel } from './ResultPanel';
import { SqlEditor } from './SqlEditor';
import './query.css';

/** A query tab: toolbar over the editor, results and messages below. */
export function QueryTab({ tab }: { tab: QueryTabRef }) {
  const doc = useQueryDocs((s) => s.docs.get(tab.id));
  const kind = useConnections((s) => (doc?.connectionId ? s.summaries.get(doc.connectionId)?.config.kind : undefined));
  const run = useQueryRun((s) => s.runs.get(tab.id) ?? emptyRun);
  const [mounted, setMounted] = useState<{ editor: EditorLike; monaco: MonacoLike } | null>(null);
  const engine = kind ?? doc?.dialect ?? 'mysql';
  useEditorMarks(mounted, engine, run);
  if (!doc) {
    return (
      <div className="empty">
        <strong>Query not found</strong>
      </div>
    );
  }
  return (
    <div className="query-tab">
      <QueryToolbar doc={doc} />
      <SqlEditor doc={doc} engine={engine} onMount={(editor, monaco) => setMounted({ editor, monaco })} />
      <ResultPanel tabId={tab.id} />
    </div>
  );
}
