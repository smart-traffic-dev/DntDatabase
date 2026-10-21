import { useLayoutEffect, useRef, useState, type CSSProperties, type DragEvent } from 'react';
import { groupTabs, useTabs } from '../store/tabs';
import { ObjectTab } from '../tabs/ObjectTab';
import { QueryTab } from '../tabs/query/QueryTab';
import { EmptyWorkspace } from './EmptyWorkspace';
import { Splitter } from './Splitter';
import { draggedTab } from './tabDrag';
import { TAB_DRAG_TYPE, TabBar, tabContentId } from './TabBar';

/** A group never shrinks below this (spec §4). */
const MIN_W = 320;
const MIN_H = 200;
/** Share of a pane's width / height that counts as its right / bottom edge for a split drop. */
const EDGE = 0.25;

export type DropZone = 'right' | 'down' | 'center';

/** Where a dragged tab lands in a group's content: its right or bottom edge splits, the rest moves. */
export function dropZone(rect: { left: number; top: number; width: number; height: number }, x: number, y: number): DropZone {
  if (rect.width <= 0 || rect.height <= 0) return 'center';
  const fx = (x - rect.left) / rect.width;
  const fy = (y - rect.top) / rect.height;
  if (fx >= 1 - EDGE && fx - (1 - EDGE) >= fy - (1 - EDGE)) return 'right';
  if (fy >= 1 - EDGE) return 'down';
  return 'center';
}

/** welcome: the workspace holds no tab at all; this (first) group shows the getting-started text. */
function GroupPane({ groupId, index, welcome }: { groupId: string; index: number; welcome: boolean }) {
  const tab = useTabs((s) => {
    const activeId = s.groups.find((g) => g.id === groupId)?.activeId;
    return s.tabs.find((t) => t.id === activeId);
  });
  const focused = useTabs((s) => s.focusedGroupId === groupId);
  const [zone, setZone] = useState<DropZone | null>(null);

  const isTabDrag = (e: DragEvent) => [...(e.dataTransfer?.types ?? [])].includes(TAB_DRAG_TYPE);
  /**
   * The zone a drop of `tabId` would really use: an edge only splits when a group can still be
   * added and the tab is not the only one of this group (it would just come back to the same place).
   */
  const zoneFor = (e: DragEvent<HTMLElement>, tabId: string | null): DropZone => {
    const zone = dropZone(e.currentTarget.getBoundingClientRect(), e.clientX, e.clientY);
    if (zone === 'center') return zone;
    const s = useTabs.getState();
    const alone = !!tabId && s.groupOf[tabId] === groupId && groupTabs(s, groupId).length === 1;
    return s.canSplit() && !alone ? zone : 'center';
  };
  const onDragOver = (e: DragEvent<HTMLElement>) => {
    if (!isTabDrag(e)) return;
    e.preventDefault();
    e.dataTransfer.dropEffect = 'move';
    setZone(zoneFor(e, draggedTab()));
  };
  const onDrop = (e: DragEvent<HTMLElement>) => {
    const id = e.dataTransfer.getData(TAB_DRAG_TYPE);
    const at = zoneFor(e, id || null);
    setZone(null);
    if (!id) return;
    e.preventDefault();
    const store = useTabs.getState();
    store.moveTab(id, groupId);
    // An edge drop splits this group with the tab in the new half.
    if (at !== 'center') useTabs.getState().splitTab(id, at);
  };

  return (
    <section
      className={`editor-group${focused ? ' focused' : ''}`}
      data-testid="editor-group"
      aria-label={`Editor group ${index + 1}`}
      onMouseDown={() => !focused && useTabs.getState().focusGroup(groupId)}
      // Keyboard focus moving into a group (Tab, the editor) focuses it too.
      onFocusCapture={() => useTabs.getState().focusedGroupId !== groupId && useTabs.getState().focusGroup(groupId)}
    >
      <TabBar groupId={groupId} />
      <div
        className="tab-content"
        id={tabContentId(groupId)}
        data-testid="group-content"
        aria-label="Tab content"
        onDragOver={onDragOver}
        onDragLeave={() => setZone(null)}
        onDrop={onDrop}
      >
        {tab?.kind === 'object' ? (
          <ObjectTab key={tab.id} tab={tab} />
        ) : tab?.kind === 'query' ? (
          <QueryTab key={tab.id} tab={tab} />
        ) : welcome ? (
          <EmptyWorkspace />
        ) : (
          <div className="empty">
            <span>Drop a tab here or open one</span>
          </div>
        )}
        {zone && <div className={`drop-zone zone-${zone}`} aria-hidden="true" />}
      </div>
    </section>
  );
}

/** Tracks an element's size (for the px values the splitters work in). */
function useSize() {
  const ref = useRef<HTMLDivElement>(null);
  const [size, setSize] = useState({ width: 0, height: 0 });
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const read = () => setSize({ width: el.clientWidth, height: el.clientHeight });
    read();
    if (typeof ResizeObserver === 'undefined') return;
    const observer = new ResizeObserver(read);
    observer.observe(el);
    return () => observer.disconnect();
  }, []);
  return [ref, size] as const;
}

/** A splitter working in a ratio of `total` px (min sizes kept on both sides). */
function RatioSplitter({ axis, total, min }: { axis: 'x' | 'y'; total: number; min: number }) {
  const ratio = useTabs((s) => s.ratios[axis]);
  // Before the first measure (and in tests) fall back to a nominal size, so the ratio still maps.
  const size = total > 0 ? total : 1000;
  const lo = Math.min(min, size / 2);
  return (
    <Splitter
      orientation={axis === 'x' ? 'vertical' : 'horizontal'}
      label="Resize editor groups"
      value={ratio * size}
      min={lo}
      max={size - lo}
      defaultValue={size / 2}
      onChange={(px) => useTabs.getState().setRatios({ [axis]: px / size })}
    />
  );
}

/**
 * The workspace: up to four editor groups, each with its own tab strip. Single, two columns, two
 * rows, or a 2 × 2 grid (with three groups the bottom one spans the width).
 */
export function EditorArea() {
  const groups = useTabs((s) => s.groups);
  const layout = useTabs((s) => s.layout);
  const ratios = useTabs((s) => s.ratios);
  const noTabs = useTabs((s) => s.tabs.length === 0);
  const [ref, size] = useSize();
  const pane = (i: number) => {
    const g = groups[i];
    return g ? <GroupPane key={g.id} groupId={g.id} index={i} welcome={i === 0 && noTabs} /> : null;
  };
  const first = (axis: 'x' | 'y'): CSSProperties => (axis === 'x' ? { flex: `0 0 ${ratios.x * 100}%` } : { flex: `0 0 ${ratios.y * 100}%` });
  const row = (a: number, b: number | null, key: string, style?: CSSProperties) => (
    <div className="editor-row" key={key} style={style}>
      {b === null ? (
        pane(a)
      ) : (
        <>
          <div className="editor-cell" style={first('x')}>
            {pane(a)}
          </div>
          <RatioSplitter axis="x" total={size.width} min={MIN_W} />
          <div className="editor-cell">{pane(b)}</div>
        </>
      )}
    </div>
  );

  return (
    <main className={`workspace editor-area layout-${layout}`} ref={ref}>
      {layout === 'single' && pane(0)}
      {layout === 'columns' && row(0, 1, 'r0', { flex: 1 })}
      {layout === 'rows' && (
        <div className="editor-col">
          <div className="editor-cell" style={first('y')}>
            {pane(0)}
          </div>
          <RatioSplitter axis="y" total={size.height} min={MIN_H} />
          <div className="editor-cell">{pane(1)}</div>
        </div>
      )}
      {layout === 'grid' && (
        <div className="editor-col">
          {row(0, 1, 'r0', first('y'))}
          <RatioSplitter axis="y" total={size.height} min={MIN_H} />
          {row(2, groups.length > 3 ? 3 : null, 'r1', { flex: 1 })}
        </div>
      )}
    </main>
  );
}
