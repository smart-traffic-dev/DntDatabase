import { getCurrentWindow, UserAttentionType } from '@tauri-apps/api/window';
import { CommandPalette } from './components/CommandPalette';
import { installRunNotifier } from './desktop/runNotifier';
import { useEffect, type CSSProperties } from 'react';
import { Splitter } from './components/Splitter';
import { SIDEBAR_DEFAULT, SIDEBAR_MAX, SIDEBAR_MIN, useLayout } from './store/layout';
import { onQueryShortcut } from './components/queryShortcuts';
import { installCloseGuard, useQueryDocs } from './store/queryTabs';
// Registers the run store as the close guard's transaction source (before the guard runs).
import './store/queryRun';
import { DialogHost } from './dialogs/DialogHost';
import { ErrorBoundary } from './components/ErrorBoundary';
import { Sidebar } from './components/Sidebar';
import { StatusBar } from './components/StatusBar';
import { EditorArea } from './components/EditorArea';
import { TitleBar } from './components/TitleBar';
import { persistArrangement, restoreArrangement } from './store/editorArrangement';
import { installRunMarks } from './store/runMarks';
import { useTabs } from './store/tabs';
import { useTree } from './store/tree';

export { EmptyWorkspace } from './components/EmptyWorkspace';

export default function App() {
  const sidebarWidth = useLayout((s) => s.sidebarWidth);
  const sidebarVisible = useLayout((s) => s.sidebarVisible);
  const setSidebarWidth = useLayout((s) => s.setSidebarWidth);
  useEffect(() => {
    let live = true;
    let stopPersist = () => {};
    void (async () => {
      await useTree.getState().reloadStored();
      // After the connections are known, so a tab bound to a deleted one comes back unbound.
      // A tree that failed to load leaves them unknown: then every binding is kept.
      await useQueryDocs.getState().restore({ connectionsKnown: !useTree.getState().error });
      // The editor groups go back around the reopened tabs; only then are changes saved again.
      restoreArrangement();
      if (live) stopPersist = persistArrangement();
    })();
    return () => {
      live = false;
      stopPersist();
    };
  }, []);

  // Result marks on query tabs whose run ended out of sight.
  useEffect(() => installRunMarks(), []);

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
          <EditorArea />
        </ErrorBoundary>
      </div>
      <StatusBar />
      <CommandPalette />
      <DialogHost />
    </div>
  );
}
