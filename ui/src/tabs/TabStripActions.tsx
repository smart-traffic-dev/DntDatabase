import { useShallow } from 'zustand/react/shallow';
import { useQueryDocs } from '../store/queryTabs';
import type { Tab } from '../store/tabs';
import { ObjectActions } from './ObjectActions';
import { QueryToolbar } from './query/QueryToolbar';

/**
 * The query cluster of a tab: subscribes to the document's toolbar fields only (never its text,
 * which changes on every keystroke).
 */
function QueryActions({ tabId }: { tabId: string }) {
  const doc = useQueryDocs(
    useShallow((s) => {
      const d = s.docs.get(tabId);
      return d ? { id: d.id, connectionId: d.connectionId, database: d.database, rowLimit: d.rowLimit, continueOnError: d.continueOnError } : null;
    }),
  );
  return doc ? <QueryToolbar doc={doc} /> : null;
}

/**
 * The action cluster at the right end of a group's tab strip, for that group's active tab
 * (editor v2 §2): the query or object tab's controls live here instead of a toolbar row.
 */
export function TabStripActions({ tab }: { groupId: string; tab: Tab | undefined }) {
  if (!tab) return null;
  // The designer and the DDL editor have no strip controls of their own (their header holds them).
  if (tab.kind === 'design' || tab.kind === 'ddl') return null;
  // Keyed by tab: an open menu or picker never carries over to the next active tab.
  return tab.kind === 'query' ? <QueryActions key={tab.id} tabId={tab.id} /> : <ObjectActions key={tab.id} tab={tab} />;
}
