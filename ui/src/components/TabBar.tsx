import type { ObjectKind } from '../bindings';
import { useConnections } from '../store/connections';
import { useQueryDocs } from '../store/queryTabs';
import { useTabs, type Tab } from '../store/tabs';
import { CloseIcon } from './icons';

const KIND_LABEL: Record<ObjectKind, string> = {
  Table: 'TBL',
  View: 'VIEW',
  MaterializedView: 'MV',
  Function: 'FN',
  Procedure: 'PROC',
  Trigger: 'TRG',
  Event: 'EVT',
  Sequence: 'SEQ',
  Type: 'TYPE',
};

export function TabBar() {
  const tabs = useTabs((s) => s.tabs);
  const activeId = useTabs((s) => s.activeId);
  const activate = useTabs((s) => s.activate);
  const close = useTabs((s) => s.close);
  const summaries = useConnections((s) => s.summaries);
  const docs = useQueryDocs((s) => s.docs);
  // A query tab closes through its guards (open transaction, unsaved file), then drops its row.
  const closeTab = (tab: Tab) => {
    if (tab.kind === 'query') void useQueryDocs.getState().closeTab(tab.id);
    else close(tab.id);
  };
  return (
    <div role="tablist" aria-label="Open tabs" className="tabbar">
      {tabs.map((tab) => {
        // Environment colour of the tab's connection, as in the tree and status bar.
        const env = (tab.connectionId ? summaries.get(tab.connectionId)?.config.env_tag : null) ?? null;
        const classes = ['tab', tab.id === activeId ? 'active' : '', env ? `env-${env}` : ''].filter(Boolean).join(' ');
        // `●` only for a tab bound to a file whose text differs from it.
        const doc = tab.kind === 'query' ? docs.get(tab.id) : undefined;
        const unsaved = !!doc?.filePath && doc.dirty;
        return (
          <div key={tab.id} className={classes}>
            <button
              type="button"
              role="tab"
              aria-selected={tab.id === activeId}
              className="tab-label"
              onClick={() => activate(tab.id)}
              onAuxClick={(e) => e.button === 1 && closeTab(tab)}
            >
              {env && <span className={`env-dot env-${env}`} title={`Environment: ${env}`} />}
              <span className="tab-kind">{tab.kind === 'query' ? 'SQL' : KIND_LABEL[tab.objectRef.kind]}</span>
              <span>{tab.title}</span>
              {unsaved && (
                <span className="tab-dirty" title="Unsaved changes" aria-label="Unsaved changes">
                  ●
                </span>
              )}
            </button>
            <button type="button" className="tab-close" aria-label={`Close ${tab.title}`} onClick={() => closeTab(tab)}>
              <CloseIcon />
            </button>
          </div>
        );
      })}
    </div>
  );
}
