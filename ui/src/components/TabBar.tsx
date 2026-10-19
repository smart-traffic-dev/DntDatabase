import type { ObjectKind } from '../bindings';
import { useConnections } from '../store/connections';
import { useTabs } from '../store/tabs';
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
  return (
    <div role="tablist" aria-label="Open tabs" className="tabbar">
      {tabs.map((tab) => {
        // Environment colour of the tab's connection, as in the tree and status bar.
        const env = summaries.get(tab.connectionId)?.config.env_tag ?? null;
        const classes = ['tab', tab.id === activeId ? 'active' : '', env ? `env-${env}` : ''].filter(Boolean).join(' ');
        return (
          <div key={tab.id} className={classes}>
            <button
              type="button"
              role="tab"
              aria-selected={tab.id === activeId}
              className="tab-label"
              onClick={() => activate(tab.id)}
              onAuxClick={(e) => e.button === 1 && close(tab.id)}
            >
              {env && <span className={`env-dot env-${env}`} title={`Environment: ${env}`} />}
              <span className="tab-kind">{KIND_LABEL[tab.objectRef.kind]}</span>
              <span>{tab.title}</span>
            </button>
            <button type="button" className="tab-close" aria-label={`Close ${tab.title}`} onClick={() => close(tab.id)}>
              <CloseIcon />
            </button>
          </div>
        );
      })}
    </div>
  );
}
