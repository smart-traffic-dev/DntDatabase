import {
  Fragment,
  useEffect,
  useMemo,
  useRef,
  useState,
  type DragEvent,
  type KeyboardEvent as ReactKeyboardEvent,
  type MouseEvent as ReactMouseEvent,
  type WheelEvent,
} from 'react';
import { useShallow } from 'zustand/react/shallow';
import type { ObjectKind } from '../bindings';
import { useConnections } from '../store/connections';
import { useQueryDocs } from '../store/queryTabs';
import { useQueryRun } from '../store/queryRun';
import { groupTabs, MAX_GROUPS, useTabs, type Tab } from '../store/tabs';
import { ActionMenu, type MenuEntry } from './ActionMenu';
import { EnvTag } from './EnvTag';
import { ChevronDownIcon, CloseIcon, LockIcon, PinIcon, PlusIcon, RestoreIcon } from './icons';
import { focusFocusedGroup } from './focusGroup';
import { openNewQuery } from './newQuery';
import { openObjectTab } from './openObject';
import { draggedItem, setDraggedTab, TAB_DRAG_TYPE, useDrag } from './tabDrag';
import { useRunMarks } from '../store/runMarks';
import { useTree } from '../store/tree';
import { TabStripActions } from '../tabs/TabStripActions';
import { closeAllTabs, closeOtherTabs, closeTab, closeTabsToRight, reopenClosedTab, togglePin } from './tabActions';

/** DOM id of a tab's button. */
export const tabDomId = (id: string) => `tab-${encodeURIComponent(id)}`;
/** DOM id of a group's tab panel (the `aria-controls` target of its tabs). */
export const tabContentId = (groupId: string) => `tab-content-${groupId}`;

export { TAB_DRAG_TYPE } from './tabDrag';

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

/** The tab strip of one editor group (by default the focused one). */
export function TabBar({ groupId: groupProp }: { groupId?: string } = {}) {
  const focusedGroupId = useTabs((s) => s.focusedGroupId);
  const groupId = groupProp ?? focusedGroupId;
  const allTabs = useTabs((s) => s.tabs);
  const groupOf = useTabs((s) => s.groupOf);
  const tabs = useMemo(() => groupTabs({ tabs: allTabs, groupOf }, groupId), [allTabs, groupOf, groupId]);
  const activeId = useTabs((s) => s.groups.find((g) => g.id === groupId)?.activeId);
  const groups = useTabs((s) => s.groups);
  const focused = groupId === focusedGroupId;
  const activate = useTabs((s) => s.activate);
  const [insertAt, setInsertAt] = useState<number | null>(null);
  const runningIds = useQueryRun(
    useShallow((s) => [...s.runs].filter(([, r]) => r.phase.kind !== 'idle').map(([id]) => id)),
  );
  const marks = useRunMarks((s) => s.marks);
  const maximized = useTabs((s) => s.maximizedGroupId === groupId);
  const summaries = useConnections((s) => s.summaries);
  // Derived id lists, not the docs / runs maps: typing replaces the docs map on every keystroke.
  // `useShallow` keeps the same array while the ids do not change, so nothing re-renders.
  const unsavedIds = useQueryDocs(
    useShallow((s) => [...s.docs.values()].filter((d) => !!d.filePath && d.dirty).map((d) => d.id)),
  );
  const txIds = useQueryRun(
    useShallow((s) => [...s.runs].filter(([, r]) => (r.txState ?? 'idle') !== 'idle').map(([id]) => id)),
  );
  const canReopen = useTabs((s) => s.closed.length > 0);
  const canSplit = groups.length < MAX_GROUPS;
  const strip = useRef<HTMLDivElement>(null);
  const [menu, setMenu] = useState<{ x: number; y: number; items: MenuEntry[] } | null>(null);

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

  const contextItems = (tab: Tab): MenuEntry[] => {
    const at = tabs.findIndex((t) => t.id === tab.id);
    // Pinned tabs are never closed in bulk.
    const others = tabs.filter((t) => t.id !== tab.id && !t.pinned);
    const right = tabs.slice(at + 1).filter((t) => !t.pinned);
    const unpinned = tabs.filter((t) => !t.pinned);
    return [
      { label: 'Close', hint: 'Ctrl+W', run: () => void closeTab(tab) },
      { label: 'Close others', disabled: others.length === 0, run: () => void closeOtherTabs(tab) },
      { label: 'Close tabs to the right', disabled: right.length === 0, run: () => void closeTabsToRight(tab) },
      { label: 'Close all', disabled: unpinned.length === 0, run: () => void closeAllTabs(groupId) },
      { separator: true },
      { label: tab.pinned ? 'Unpin' : 'Pin', run: () => togglePin(tab) },
      { separator: true },
      { label: 'Split right', hint: 'Ctrl+\\', disabled: !canSplit, run: () => (useTabs.getState().splitTab(tab.id, 'right'), focusFocusedGroup()) },
      { label: 'Split down', hint: 'Ctrl+K Ctrl+\\', disabled: !canSplit, run: () => (useTabs.getState().splitTab(tab.id, 'down'), focusFocusedGroup()) },
      ...groups
        .map((g, i) => ({ g, i }))
        .filter(({ g }) => g.id !== groupId)
        .map(({ g, i }) => ({ label: `Move to group ${i + 1}`, run: () => (useTabs.getState().moveTab(tab.id, g.id), focusFocusedGroup()) })),
      { label: 'Reopen closed tab', hint: 'Ctrl+Shift+T', disabled: !canReopen, run: () => void reopenClosedTab() },
      { separator: true },
      { label: 'Copy title', run: () => void navigator.clipboard?.writeText(tab.title).catch(() => undefined) },
      ...(tab.kind === 'query'
        ? [{ label: 'Save', hint: 'Ctrl+S', run: () => void useQueryDocs.getState().saveFile(tab.id, false) }]
        : []),
    ];
  };

  // WebView2 still fires `contextmenu` (at 0,0) after the key's keydown; it must not replace the menu
  // the key just opened under the tab.
  const keyMenuAt = useRef(0);
  const openContext = (e: ReactMouseEvent, tab: Tab) => {
    e.preventDefault();
    if (Date.now() - keyMenuAt.current < 300) return;
    setMenu({ x: e.clientX, y: e.clientY, items: contextItems(tab) });
  };

  // Tab strip keyboard model (WAI-ARIA tabs): one tab stop, arrows / Home / End move and activate,
  // the context-menu key or Shift+F10 opens the tab's menu under it.
  const onTabKeyDown = (e: ReactKeyboardEvent<HTMLButtonElement>, tab: Tab, index: number) => {
    const focusAt = (next: number) => {
      const target = tabs[(next + tabs.length) % tabs.length];
      if (!target) return;
      e.preventDefault();
      activate(target.id);
      strip.current?.querySelectorAll<HTMLElement>('[role="tab"]')[(next + tabs.length) % tabs.length]?.focus();
    };
    if (e.key === 'ArrowRight') return focusAt(index + 1);
    if (e.key === 'ArrowLeft') return focusAt(index - 1);
    if (e.key === 'Home') return focusAt(0);
    if (e.key === 'End') return focusAt(tabs.length - 1);
    if (e.key === 'ContextMenu' || (e.shiftKey && e.key === 'F10')) {
      e.preventDefault();
      keyMenuAt.current = Date.now();
      const r = e.currentTarget.getBoundingClientRect();
      setMenu({ x: r.left, y: r.bottom, items: contextItems(tab) });
    }
  };
  // The roving tab stop: the active tab, else the first one.
  const stopId = tabs.some((t) => t.id === activeId) ? activeId : tabs[0]?.id;

  // Dragging over the strip: a tab lands where the insertion bar shows (in this strip or from another
  // group's), a tree object opens in this group.
  const dragging = useDrag((s) => s.item);
  /** The strip position under the pointer: before the first tab whose middle is right of it. */
  const insertIndex = (x: number) => {
    const els = [...(strip.current?.querySelectorAll<HTMLElement>('.tab') ?? [])];
    const at = els.findIndex((el) => {
      const r = el.getBoundingClientRect();
      return x < r.left + r.width / 2;
    });
    return at < 0 ? els.length : at;
  };
  const onDrop = (e: DragEvent<HTMLDivElement>) => {
    const item = draggedItem();
    const at = insertAt ?? insertIndex(e.clientX);
    setInsertAt(null);
    if (!item) return;
    e.preventDefault();
    useDrag.setState({ item: null });
    if (item.kind === 'tab') {
      // The index counts this group's tabs without the dragged one.
      const from = tabs.findIndex((t) => t.id === item.id);
      useTabs.getState().moveTab(item.id, groupId, from >= 0 && from < at ? at - 1 : at);
    } else {
      const node = useTree.getState().nodes.get(item.nodeId);
      if (!node) return;
      useTabs.getState().focusGroup(groupId);
      openObjectTab(node);
    }
    focusFocusedGroup();
  };

  return (
    <div
      className={`tabstrip${focused ? '' : ' unfocused'}`}
      onMouseDown={() => !focused && useTabs.getState().focusGroup(groupId)}
      onDragOver={(e) => {
        if (!draggedItem()) return;
        e.preventDefault();
        e.dataTransfer.dropEffect = 'move';
        setInsertAt(draggedItem()?.kind === 'tab' ? insertIndex(e.clientX) : null);
      }}
      onDragLeave={(e) => {
        if (!e.currentTarget.contains(e.relatedTarget as Node | null)) setInsertAt(null);
      }}
      onDrop={onDrop}
    >
      <div role="tablist" aria-label="Open tabs" className="tabbar" ref={strip} onWheel={onWheel}>
        {tabs.map((tab, index) => {
          const conn = tab.connectionId ? summaries.get(tab.connectionId)?.config : undefined;
          // Environment colour of the tab's connection, as in the tree and status bar.
          const env = conn?.env_tag ?? null;
          const readOnly = !!conn?.read_only;
          const classes = ['tab', tab.id === activeId ? 'active' : '', env ? `env-${env}` : '', tab.preview ? 'preview' : '']
            .filter(Boolean)
            .join(' ');
          const running = runningIds.includes(tab.id);
          const mark = marks.get(tab.id);
          // `●` only for a tab bound to a file whose text differs from it.
          const unsaved = tab.kind === 'query' && unsavedIds.includes(tab.id);
          const inTx = tab.kind === 'query' && txIds.includes(tab.id);
          const tip = [tab.title, conn ? `Connection: ${conn.name}` : null, readOnly ? 'Read-only' : null, inTx ? 'Open transaction' : null]
            .filter(Boolean)
            .join(' · ');
          return (
            <Fragment key={tab.id}>
            {dragging && insertAt === index && <span className="tab-insert" aria-hidden="true" />}
            <div
              className={classes}
              data-tab-id={tab.id}
              title={tip}
              draggable
              onDragStart={(e) => {
                setDraggedTab(tab.id);
                e.dataTransfer.setData(TAB_DRAG_TYPE, tab.id);
                e.dataTransfer.effectAllowed = 'move';
              }}
              onDragEnd={() => {
                setDraggedTab(null);
                setInsertAt(null);
              }}
              onContextMenu={(e) => openContext(e, tab)}
            >
              <button
                type="button"
                role="tab"
                id={tabDomId(tab.id)}
                aria-selected={tab.id === activeId}
                aria-controls={tabContentId(groupId)}
                tabIndex={tab.id === stopId ? 0 : -1}
                className="tab-label"
                onClick={() => activate(tab.id)}
                // A preview tab is kept by a double-click; a kept tab's double-click maximizes its group.
                onDoubleClick={() => (tab.preview ? useTabs.getState().keepTab(tab.id) : useTabs.getState().toggleMaximize(groupId))}
                onAuxClick={(e) => e.button === 1 && void closeTab(tab)}
                onKeyDown={(e) => onTabKeyDown(e, tab, index)}
              >
                {tab.pinned && (
                  <span className="tab-pin" title="Pinned" role="img" aria-label="Pinned">
                    <PinIcon />
                  </span>
                )}
                {env && <EnvTag env={env} />}
                <span className="tab-kind">{tab.kind === 'query' ? 'SQL' : KIND_LABEL[tab.objectRef.kind]}</span>
                <span className="tab-title">{tab.title}</span>
                {readOnly && (
                  <span className="tab-ro" title="Read-only connection" aria-label="Read-only connection">
                    <LockIcon />
                  </span>
                )}
                {running && (
                  <span className="tab-run" title="Running" aria-label="Running" role="img">
                    <span className="spinner inline" aria-hidden="true" />
                  </span>
                )}
                {!running && mark && (
                  <span
                    className={`tab-mark ${mark}`}
                    title={mark === 'ok' ? 'Finished' : 'Failed'}
                    aria-label={mark === 'ok' ? 'Finished' : 'Failed'}
                    role="img"
                  />
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
              {/* Not a tab stop: the keyboard closes a tab with Ctrl+W or its menu. */}
              <button type="button" className="tab-close" tabIndex={-1} aria-label={`Close ${tab.title}`} onClick={() => void closeTab(tab)}>
                <CloseIcon />
              </button>
            </div>
            </Fragment>
          );
        })}
        {dragging && insertAt === tabs.length && <span className="tab-insert" aria-hidden="true" />}
      </div>
      <button
        type="button"
        className="icon-btn tab-new"
        data-testid="new-query"
        aria-label="New query"
        title="New query (Ctrl+T)"
        onClick={() => openNewQuery()}
      >
        <PlusIcon />
      </button>
      <span className="tabstrip-fill" />
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
      {maximized && (
        <span className="tab-maximized">
          Maximized
          <button
            type="button"
            className="icon-btn"
            aria-label="Restore editor layout (Ctrl+K Ctrl+M)"
            title="Restore editor layout (Ctrl+K Ctrl+M)"
            onClick={() => useTabs.getState().toggleMaximize(groupId)}
          >
            <RestoreIcon />
          </button>
        </span>
      )}
      <TabStripActions groupId={groupId} tab={tabs.find((t) => t.id === activeId)} />
      {menu && <ActionMenu label="Tabs" checkRole="radio" x={menu.x} y={menu.y} items={menu.items} onClose={() => setMenu(null)} />}
    </div>
  );
}
