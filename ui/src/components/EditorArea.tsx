import { useEffect, useLayoutEffect, useRef, useState, type DragEvent } from 'react';
import { groupTabs, useTabs, type LayoutNode, type Side } from '../store/tabs';
import { useTree } from '../store/tree';
import { DdlEditorTab } from '../tabs/ddl/DdlEditorTab';
import { DesignerTab } from '../tabs/designer/DesignerTab';
import { ObjectTab } from '../tabs/ObjectTab';
import { QueryTab } from '../tabs/query/QueryTab';
import { EmptyWorkspace } from './EmptyWorkspace';
import { focusFocusedGroup } from './focusGroup';
import { openObjectTab } from './openObject';
import { Splitter } from './Splitter';
import { OBJECT_DRAG_TYPE, openTabOfNode, TAB_DRAG_TYPE, useDrag, type DragItem } from './tabDrag';
import { TabBar, tabContentId } from './TabBar';

/** A group never shrinks below this when the window allows it (spec §1). */
const MIN_W = 320;
const MIN_H = 200;
/** The outer third of a pane on each side splits that side. */
const EDGE = 1 / 3;

export type DropZone = Side | 'center';

/** Where a drop lands in a pane: the nearest edge when the pointer is in its outer third, else the middle. */
export function dropZone(rect: { left: number; top: number; width: number; height: number }, x: number, y: number): DropZone {
  if (rect.width <= 0 || rect.height <= 0) return 'center';
  const fx = (x - rect.left) / rect.width;
  const fy = (y - rect.top) / rect.height;
  if (!Number.isFinite(fx) || !Number.isFinite(fy)) return 'center';
  const near: [DropZone, number][] = [
    ['left', fx],
    ['right', 1 - fx],
    ['up', fy],
    ['down', 1 - fy],
  ];
  const [side, distance] = near.reduce((best, cur) => (cur[1] < best[1] ? cur : best));
  return distance < EDGE ? side : 'center';
}

const ZONE_LABEL: Record<DropZone, string> = { left: 'Split left', right: 'Split right', up: 'Split up', down: 'Split down', center: 'Move here' };
const OBJECT_LABEL: Record<DropZone, string> = {
  left: 'Open to the left',
  right: 'Open to the right',
  up: 'Open above',
  down: 'Open below',
  center: 'Open here',
};

/** Drops a tab or a tree object on `groupId` at `zone`. */
function drop(item: DragItem, groupId: string, zone: DropZone) {
  const tabs = useTabs.getState();
  const tabId = item.kind === 'tab' ? item.id : openTabOfNode(item.nodeId);
  if (tabId) {
    // The middle of its own group: nothing to do (it would only move to the strip's end).
    if (zone === 'center' && tabs.groupOf[tabId] === groupId) tabs.activate(tabId);
    else if (zone === 'center') tabs.moveTab(tabId, groupId);
    else tabs.splitGroup(groupId, zone, tabId);
  } else if (item.kind === 'object') {
    const node = useTree.getState().nodes.get(item.nodeId);
    if (!node) return;
    if (zone === 'center') tabs.focusGroup(groupId);
    else tabs.splitGroup(groupId, zone);
    openObjectTab(node);
  }
  focusFocusedGroup();
}

/**
 * Covers a group's content while something is dragged: above the editor and the grid, so a drop
 * always lands here. Shows the half that will become the new group, or the whole pane for a move.
 */
function DropLayer({ groupId, item }: { groupId: string; item: DragItem }) {
  const [zone, setZone] = useState<DropZone | null>(null);
  /** The zone a drop would really use: an edge only when a split can happen. */
  const zoneAt = (e: DragEvent<HTMLElement>): DropZone => {
    const z = dropZone(e.currentTarget.getBoundingClientRect(), e.clientX, e.clientY);
    if (z === 'center') return z;
    const s = useTabs.getState();
    const tabId = item.kind === 'tab' ? item.id : openTabOfNode(item.nodeId);
    const alone = !!tabId && s.groupOf[tabId] === groupId && groupTabs(s, groupId).length === 1;
    return s.canSplit() && !alone ? z : 'center';
  };
  return (
    <div
      className="drop-layer"
      data-testid="drop-layer"
      onDragOver={(e) => {
        e.preventDefault();
        e.dataTransfer.dropEffect = 'move';
        setZone(zoneAt(e));
      }}
      onDragLeave={() => setZone(null)}
      onDrop={(e) => {
        e.preventDefault();
        const at = zoneAt(e);
        setZone(null);
        useDrag.setState({ item: null });
        drop(item, groupId, at);
      }}
    >
      {zone && (
        <div className={`drop-zone zone-${zone}`} aria-hidden="true">
          <span className="drop-label">{(item.kind === 'tab' ? ZONE_LABEL : OBJECT_LABEL)[zone]}</span>
        </div>
      )}
    </div>
  );
}

/** `welcome`: the workspace holds no tab at all; this (first) group shows the getting-started text. */
function GroupPane({ groupId, index, welcome }: { groupId: string; index: number; welcome: boolean }) {
  const tab = useTabs((s) => {
    const activeId = s.groups.find((g) => g.id === groupId)?.activeId;
    return s.tabs.find((t) => t.id === activeId);
  });
  const focused = useTabs((s) => s.focusedGroupId === groupId);
  const item = useDrag((s) => s.item);

  return (
    <section
      className={`editor-group${focused ? ' focused' : ''}`}
      data-testid="editor-group"
      data-group-id={groupId}
      aria-label={`Editor group ${index + 1}`}
      onMouseDown={() => !focused && useTabs.getState().focusGroup(groupId)}
      // Keyboard focus moving into a group (Tab, the editor) focuses it too.
      onFocusCapture={() => useTabs.getState().focusedGroupId !== groupId && useTabs.getState().focusGroup(groupId)}
    >
      <TabBar groupId={groupId} />
      <div className="tab-content" id={tabContentId(groupId)} data-testid="group-content" aria-label="Tab content">
        {tab?.kind === 'object' ? (
          <ObjectTab key={tab.id} tab={tab} />
        ) : tab?.kind === 'query' ? (
          <QueryTab key={tab.id} tab={tab} />
        ) : tab?.kind === 'design' ? (
          <DesignerTab key={tab.id} tab={tab} />
        ) : tab?.kind === 'ddl' ? (
          <DdlEditorTab key={tab.id} tab={tab} />
        ) : welcome ? (
          <EmptyWorkspace />
        ) : (
          <div className="empty">
            <span>Drop a tab here or open one</span>
          </div>
        )}
        {item && <DropLayer groupId={groupId} item={item} />}
      </div>
    </section>
  );
}

/** Tracks an element's size along one axis (the px values a splitter works in). */
function useAxisSize(axis: 'x' | 'y') {
  const ref = useRef<HTMLDivElement>(null);
  const [size, setSize] = useState(0);
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const read = () => setSize(axis === 'x' ? el.clientWidth : el.clientHeight);
    read();
    if (typeof ResizeObserver === 'undefined') return;
    const observer = new ResizeObserver(read);
    observer.observe(el);
    return () => observer.disconnect();
  }, [axis]);
  return [ref, size] as const;
}

/** One split node: its two sides and the splitter between them (its own ratio). */
function SplitView({ node, render }: { node: Extract<LayoutNode, { kind: 'split' }>; render(n: LayoutNode): React.ReactNode }) {
  const axis = node.dir === 'row' ? 'x' : 'y';
  const [ref, measured] = useAxisSize(axis);
  // Before the first measure (and in tests) a nominal size keeps the ratio mapping.
  const size = measured > 0 ? measured : 1000;
  const min = Math.min(axis === 'x' ? MIN_W : MIN_H, size / 2);
  return (
    <div ref={ref} className={`editor-split ${node.dir}`}>
      <div className="editor-cell" style={{ flex: `${node.ratio} 1 0` }}>
        {render(node.a)}
      </div>
      <Splitter
        orientation={node.dir === 'row' ? 'vertical' : 'horizontal'}
        label="Resize editor groups"
        value={node.ratio * size}
        min={min}
        max={size - min}
        defaultValue={size / 2}
        onChange={(px) => useTabs.getState().setSplitRatio(node.id, px / size)}
      />
      <div className="editor-cell" style={{ flex: `${1 - node.ratio} 1 0` }}>
        {render(node.b)}
      </div>
    </div>
  );
}

/**
 * The workspace: up to four editor groups laid out by a split tree (editor v2 §1), each with its
 * own tab strip. A maximized group is shown alone; the tree stays for the restore.
 */
export function EditorArea() {
  const tree = useTabs((s) => s.tree);
  const groups = useTabs((s) => s.groups);
  const maximized = useTabs((s) => s.maximizedGroupId);
  const noTabs = useTabs((s) => s.tabs.length === 0);

  // A drag that ends anywhere (dropped elsewhere, Esc, outside the window) takes the drop layers away.
  useEffect(() => {
    const end = () => useDrag.getState().item && useDrag.setState({ item: null });
    window.addEventListener('dragend', end, true);
    window.addEventListener('drop', end);
    // No pointer press happens during a drag: one means the drag ended without a `dragend`
    // reaching the window (its source left the DOM, e.g. a recycled virtual tree row).
    window.addEventListener('mousedown', end, true);
    return () => {
      window.removeEventListener('dragend', end, true);
      window.removeEventListener('drop', end);
      window.removeEventListener('mousedown', end, true);
    };
  }, []);

  const pane = (id: string) => {
    const index = groups.findIndex((g) => g.id === id);
    return <GroupPane key={id} groupId={id} index={index} welcome={index === 0 && noTabs} />;
  };
  const render = (n: LayoutNode): React.ReactNode => (n.kind === 'group' ? pane(n.id) : <SplitView key={n.id} node={n} render={render} />);

  return (
    <main className={`workspace editor-area${maximized ? ' maximized' : ''}`}>
      {maximized && groups.some((g) => g.id === maximized) ? pane(maximized) : render(tree)}
    </main>
  );
}

export { OBJECT_DRAG_TYPE, TAB_DRAG_TYPE };
