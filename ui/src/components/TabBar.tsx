import { useEffect, useRef, useState, type MouseEvent as ReactMouseEvent, type WheelEvent } from 'react';
import type { ObjectKind } from '../bindings';
import { useConnections } from '../store/connections';
import { useQueryDocs } from '../store/queryTabs';
import { useQueryRun } from '../store/queryRun';
import { useTabs, type Tab } from '../store/tabs';
import { EnvTag } from './EnvTag';
import { ChevronDownIcon, CloseIcon, LockIcon } from './icons';
import { closeTab, closeTabs } from './tabActions';
import { TabMenu, type TabMenuItem } from './TabMenu';

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
  const summaries = useConnections((s) => s.summaries);
  const docs = useQueryDocs((s) => s.docs);
  const runs = useQueryRun((s) => s.runs);
  const strip = useRef<HTMLDivElement>(null);
  const [menu, setMenu] = useState<{ x: number; y: number; items: TabMenuItem[] } | null>(null);

  // A new or activated tab is always visible, even when the strip overflows.
  useEffect(() => {
    if (!activeId) return;
    const el = [...(strip.current?.querySelectorAll<HTMLElement>('.tab') ?? [])].find((t) => t.dataset.tabId === activeId);
    el?.scrollIntoView?.({ block: 'nearest', inline: 'nearest' });
  }, [activeId, tabs.length]);

  // The scrollbar is hidden (it shifted the layout), so the wheel scrolls the strip sideways.
  const onWheel = (e: WheelEvent<HTMLDivElement>) => {
    if (e.deltaY !== 0 && e.deltaX === 0) e.currentTarget.scrollLeft += e.deltaY;
  };

  const openOverflow = (button: HTMLElement) => {
    const r = button.getBoundingClientRect();
    setMenu({
      x: Math.max(8, r.right - 220),
      y: r.bottom,
      items: tabs.map((tab) => ({
        label: tab.title,
        checked: tab.id === activeId,
        run: () => activate(tab.id),
      })),
    });
  };

  const openContext = (e: ReactMouseEvent, tab: Tab) => {
    e.preventDefault();
    const at = tabs.findIndex((t) => t.id === tab.id);
    setMenu({
      x: e.clientX,
      y: e.clientY,
      items: [
        { label: 'Close', run: () => void closeTab(tab) },
        { label: 'Close others', disabled: tabs.length < 2, run: () => void closeTabs(tabs.filter((t) => t.id !== tab.id)) },
        { label: 'Close tabs to the right', disabled: at >= tabs.length - 1, run: () => void closeTabs(tabs.slice(at + 1)) },
      ],
    });
  };

  return (
    <div className="tabstrip">
      <div role="tablist" aria-label="Open tabs" className="tabbar" ref={strip} onWheel={onWheel}>
        {tabs.map((tab) => {
          const conn = tab.connectionId ? summaries.get(tab.connectionId)?.config : undefined;
          // Environment colour of the tab's connection, as in the tree and status bar.
          const env = conn?.env_tag ?? null;
          const readOnly = !!conn?.read_only;
          const classes = ['tab', tab.id === activeId ? 'active' : '', env ? `env-${env}` : ''].filter(Boolean).join(' ');
          // `●` only for a tab bound to a file whose text differs from it.
          const doc = tab.kind === 'query' ? docs.get(tab.id) : undefined;
          const unsaved = !!doc?.filePath && doc.dirty;
          const inTx = tab.kind === 'query' && (runs.get(tab.id)?.txState ?? 'idle') !== 'idle';
          const tip = [tab.title, conn ? `Connection: ${conn.name}` : null, readOnly ? 'Read-only' : null, inTx ? 'Open transaction' : null]
            .filter(Boolean)
            .join(' · ');
          return (
            <div key={tab.id} className={classes} data-tab-id={tab.id} title={tip} onContextMenu={(e) => openContext(e, tab)}>
              <button
                type="button"
                role="tab"
                aria-selected={tab.id === activeId}
                className="tab-label"
                onClick={() => activate(tab.id)}
                onAuxClick={(e) => e.button === 1 && void closeTab(tab)}
              >
                {env && <EnvTag env={env} />}
                <span className="tab-kind">{tab.kind === 'query' ? 'SQL' : KIND_LABEL[tab.objectRef.kind]}</span>
                <span className="tab-title">{tab.title}</span>
                {readOnly && (
                  <span className="tab-ro" title="Read-only connection" aria-label="Read-only connection">
                    <LockIcon />
                  </span>
                )}
                {inTx && (
                  <span className="tab-tx" title="Open transaction" aria-label="Open transaction" role="img" />
                )}
                {unsaved && (
                  <span className="tab-dirty" title="Unsaved changes" aria-label="Unsaved changes">
                    ●
                  </span>
                )}
              </button>
              <button type="button" className="tab-close" aria-label={`Close ${tab.title}`} onClick={() => void closeTab(tab)}>
                <CloseIcon />
              </button>
            </div>
          );
        })}
      </div>
      {tabs.length > 1 && (
        <button
          type="button"
          className="tab-overflow"
          aria-label="List all tabs"
          title="List all tabs"
          aria-haspopup="menu"
          onClick={(e) => openOverflow(e.currentTarget)}
        >
          <ChevronDownIcon />
        </button>
      )}
      {menu && <TabMenu label="Tabs" x={menu.x} y={menu.y} items={menu.items} onClose={() => setMenu(null)} />}
    </div>
  );
}
