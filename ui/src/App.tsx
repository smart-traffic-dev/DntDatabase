import { getCurrentWindow } from '@tauri-apps/api/window';
import { useEffect } from 'react';
import { onQueryShortcut } from './components/queryShortcuts';
import { installCloseGuard, useQueryDocs } from './store/queryTabs';
// Registers the run store as the close guard's transaction source (before the guard runs).
import './store/queryRun';
import { DialogHost } from './dialogs/DialogHost';
import { ErrorBoundary } from './components/ErrorBoundary';
import { Sidebar } from './components/Sidebar';
import { StatusBar } from './components/StatusBar';
import { TabBar } from './components/TabBar';
import { Toolbar } from './components/Toolbar';
import { ObjectTab } from './tabs/ObjectTab';
import { QueryTab } from './tabs/query/QueryTab';
import { useTabs } from './store/tabs';
import { useTree } from './store/tree';

function Workspace() {
  const tab = useTabs((s) => s.tabs.find((t) => t.id === s.activeId));
  return (
    <main className="workspace">
      <TabBar />
      <section className="tab-content" aria-label="Tab content">
        {tab?.kind === 'object' ? (
          <ObjectTab key={tab.id} tab={tab} />
        ) : tab?.kind === 'query' ? (
          <QueryTab key={tab.id} tab={tab} />
        ) : (
          <div className="empty">
            <strong>No object open</strong>
            <span>Double-click a table or view in the connection tree.</span>
          </div>
        )}
      </section>
    </main>
  );
}

export default function App() {
  useEffect(() => {
    void (async () => {
      await useTree.getState().reloadStored();
      // After the connections are known, so a tab bound to a deleted one comes back unbound.
      await useQueryDocs.getState().restore();
    })();
  }, []);

  useEffect(() => {
    // Capture: the shortcuts work inside the editor too.
    window.addEventListener('keydown', onQueryShortcut, true);
    return () => window.removeEventListener('keydown', onQueryShortcut, true);
  }, []);

  useEffect(() => {
    let unlisten: Promise<() => void>;
    try {
      unlisten = installCloseGuard(getCurrentWindow());
    } catch {
      return; // Not inside Tauri (plain browser preview).
    }
    return () => {
      void unlisten.then((f) => f()).catch(() => {});
    };
  }, []);

  return (
    <div className="app">
      <Toolbar />
      <div className="body">
        <ErrorBoundary label="Sidebar">
          <Sidebar />
        </ErrorBoundary>
        <ErrorBoundary label="Workspace">
          <Workspace />
        </ErrorBoundary>
      </div>
      <StatusBar />
      <DialogHost />
    </div>
  );
}
