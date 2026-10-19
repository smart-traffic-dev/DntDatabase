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
  // Closing a query tab drops its document (unsaved-changes prompts come with later tasks).
  const closeTab = (tab: Tab) => {
    close(tab.id);
    if (tab.kind === 'query') useQueryDocs.getState().remove(tab.id);
  };
  return (
    <div role="tablist" aria-label="Open tabs" className="tabbar">
      {tabs.map((tab) => {
        // Environment colour of the tab's connection, as in the tree and status bar.
        const env = (tab.connectionId ? summaries.get(tab.connectionId)?.config.env_tag : null) ?? null;
        const classes = ['tab', tab.id === activeId ? 'active' : '', env ? `env-${env}` : ''].filter(Boolean).join(' ');
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
