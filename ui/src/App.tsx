import { getCurrentWindow, UserAttentionType } from '@tauri-apps/api/window';
import { CommandPalette } from './components/CommandPalette';
import { installRunNotifier } from './desktop/runNotifier';
import { useEffect, type CSSProperties } from 'react';
import { Splitter } from './components/Splitter';
import { SIDEBAR_DEFAULT, SIDEBAR_MAX, SIDEBAR_MIN, useLayout } from './store/layout';
import { openNewQuery } from './components/newQuery';
import { onQueryShortcut } from './components/queryShortcuts';
import { installCloseGuard, useQueryDocs } from './store/queryTabs';
// Registers the run store as the close guard's transaction source (before the guard runs).
import './store/queryRun';
import { DialogHost } from './dialogs/DialogHost';
import { ErrorBoundary } from './components/ErrorBoundary';
import { Sidebar } from './components/Sidebar';
import { StatusBar } from './components/StatusBar';
import { TabBar } from './components/TabBar';
import { TitleBar } from './components/TitleBar';
import { ObjectTab } from './tabs/ObjectTab';
import { QueryTab } from './tabs/query/QueryTab';
import { useConnections } from './store/connections';
import { useDialogs } from './store/dialogs';
import { useTabs } from './store/tabs';
import { useTree } from './store/tree';

/** Nothing open: a first-start call to action, else the keys that get a user going. */
export function EmptyWorkspace() {
  const hasConnections = useConnections((s) => s.summaries.size > 0);
  const requestDialog = useDialogs((s) => s.requestDialog);
  if (!hasConnections) {
    return (
      <div className="empty" data-testid="workspace-empty">
        <strong>No connections yet</strong>
        <span>Add a connection to start browsing and querying.</span>
        <button type="button" className="btn primary" onClick={() => requestDialog({ kind: 'newConnection' })}>
          New connection
        </button>
      </div>
    );
  }
  return (
    <div className="empty" data-testid="workspace-empty">
      <strong>No tab open</strong>
      <span>Double-click a table or view in the connection tree to open it.</span>
      <span>
        <kbd>Ctrl+T</kbd> opens a new query; <kbd>Ctrl+Enter</kbd> runs the statement at the cursor.
      </span>
      <button type="button" className="btn" onClick={() => openNewQuery()}>
        New query
      </button>
    </div>
  );
}

function Workspace() {
  const tab = useTabs((s) => s.tabs.find((t) => t.id === s.activeId));
  return (
    <main className="workspace">
      <TabBar />
      <section className="tab-content" id="tab-content" aria-label="Tab content">
        {tab?.kind === 'object' ? (
          <ObjectTab key={tab.id} tab={tab} />
        ) : tab?.kind === 'query' ? (
          <QueryTab key={tab.id} tab={tab} />
        ) : (
          <EmptyWorkspace />
        )}
      </section>
    </main>
  );
}

export default function App() {
  const sidebarWidth = useLayout((s) => s.sidebarWidth);
  const sidebarVisible = useLayout((s) => s.sidebarVisible);
  const setSidebarWidth = useLayout((s) => s.setSidebarWidth);
  useEffect(() => {
    void (async () => {
      await useTree.getState().reloadStored();
      // After the connections are known, so a tab bound to a deleted one comes back unbound.
      // A tree that failed to load leaves them unknown: then every binding is kept.
      await useQueryDocs.getState().restore({ connectionsKnown: !useTree.getState().error });
    })();
  }, []);

  useEffect(() => {
    // Capture: the shortcuts work inside the editor too.
    window.addEventListener('keydown', onQueryShortcut, true);
    return () => window.removeEventListener('keydown', onQueryShortcut, true);
  }, []);

  // A long run that ends while the window is in the background flashes the taskbar and shows in the title.
  useEffect(() => {
    let win: ReturnType<typeof getCurrentWindow>;
    try {
      win = getCurrentWindow();
    } catch {
      return; // Not inside Tauri (plain browser preview).
    }
    return installRunNotifier({
      win,
      attention: UserAttentionType.Informational,
      baseTitle: 'DntDatabase',
      titleOf: (id) => useTabs.getState().tabs.find((t) => t.id === id)?.title,
      hasFocus: () => document.hasFocus(),
      now: () => Date.now(),
      onFocus: (listener) => {
        window.addEventListener('focus', listener);
        return () => window.removeEventListener('focus', listener);
      },
    });
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
    <div className="app" style={{ '--sidebar-width': `${sidebarWidth}px` } as CSSProperties}>
      <TitleBar />
      <div className="body">
        {sidebarVisible && (
          <>
            <ErrorBoundary label="Sidebar">
              <Sidebar />
            </ErrorBoundary>
            <Splitter
              orientation="vertical"
              label="Resize sidebar"
              value={sidebarWidth}
              min={SIDEBAR_MIN}
              max={SIDEBAR_MAX}
              defaultValue={SIDEBAR_DEFAULT}
              onChange={setSidebarWidth}
            />
          </>
        )}
        <ErrorBoundary label="Workspace">
          <Workspace />
        </ErrorBoundary>
      </div>
      <StatusBar />
      <CommandPalette />
      <DialogHost />
    </div>
  );
}
