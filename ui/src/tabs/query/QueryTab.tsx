import { useConnections } from '../../store/connections';
import { useQueryDocs } from '../../store/queryTabs';
import type { QueryTabRef } from '../../store/tabs';
import { QueryToolbar } from './QueryToolbar';
import { SqlEditor } from './SqlEditor';
import './query.css';

/** A query tab: toolbar over the editor. Results and messages arrive with later tasks. */
export function QueryTab({ tab }: { tab: QueryTabRef }) {
  const doc = useQueryDocs((s) => s.docs.get(tab.id));
  const kind = useConnections((s) => (doc?.connectionId ? s.summaries.get(doc.connectionId)?.config.kind : undefined));
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
      <SqlEditor doc={doc} engine={kind ?? doc.dialect} />
    </div>
  );
}
