import { useVirtualizer } from '@tanstack/react-virtual';
import { useCallback, useEffect, useMemo, useRef, useState, type KeyboardEvent, type MouseEvent } from 'react';
import { useTree, type TreeNode, type TreeRow } from '../store/tree';
import { computeRows } from '../store/treeModel';
import {
  ChevronIcon,
  DatabaseIcon,
  EyeIcon,
  FolderIcon,
  ListIcon,
  ObjectIcon,
  SchemaIcon,
  TableIcon,
} from './icons';
import { openObjectTab } from './openObject';
import { TreeContextMenu } from './TreeContextMenu';

export const ROW_HEIGHT = 26;

const domId = (key: string) => `tn-${encodeURIComponent(key)}`;

function NodeIcon({ node }: { node: TreeNode }) {
  switch (node.kind) {
    case 'group':
      return <FolderIcon />;
    case 'database':
      return <DatabaseIcon />;
    case 'schema':
      return <SchemaIcon />;
    case 'folder':
      return <ListIcon />;
    case 'object':
      if (node.objectRef?.kind === 'Table') return <TableIcon />;
      if (node.objectRef?.kind === 'View' || node.objectRef?.kind === 'MaterializedView') return <EyeIcon />;
      return <ObjectIcon />;
    default:
      return null;
  }
}

interface RowProps {
  row: TreeRow;
  selected: boolean;
  dropTarget: boolean;
  style: React.CSSProperties;
  onSelect(node: TreeNode): void;
  onToggle(node: TreeNode): void;
  onOpen(node: TreeNode): void;
  onMenu(node: TreeNode, e: MouseEvent): void;
  onDragStart(node: TreeNode): void;
  onDragEnd(): void;
  onDragOver(node: TreeNode, e: React.DragEvent): void;
  onDragLeave(): void;
  onDrop(node: TreeNode, e: React.DragEvent): void;
}

function TreeRowView({ row, selected, dropTarget, style, ...on }: RowProps) {
  const { node } = row;
  const indent = { paddingLeft: 6 + (row.level - 1) * 14 };

  if (row.type === 'loading') {
    return (
      <div role="treeitem" aria-level={row.level} aria-disabled="true" aria-selected="false" data-testid="tree-row" className="tree-row placeholder" style={{ ...style, ...indent }}>
        <span className="chev" />
        <span className="muted">Loading…</span>
      </div>
    );
  }
  if (row.type === 'error') {
    const error = node.error;
    return (
      <div role="treeitem" aria-level={row.level} aria-disabled="true" aria-selected="false" data-testid="tree-row" className="tree-row placeholder error" style={{ ...style, ...indent }}>
        <span className="chev" />
        <span className="error-text" title={error?.detail ?? error?.hint ?? undefined}>
          {error?.message}
        </span>
        <button
          type="button"
          className="btn small"
          onClick={(e) => {
            e.stopPropagation();
            void useTree.getState().loadChildren(node.id);
          }}
        >
          Retry
        </button>
      </div>
    );
  }

  const draggable = node.kind === 'group' || node.kind === 'connection';
  return (
    <div
      id={domId(row.key)}
      role="treeitem"
      aria-level={row.level}
      aria-expanded={row.expandable ? row.expanded : undefined}
      aria-selected={selected}
      data-testid="tree-row"
      className={`tree-row kind-${node.kind}${selected ? ' selected' : ''}${dropTarget ? ' drop-target' : ''}`}
      style={{ ...style, ...indent }}
      draggable={draggable}
      onClick={() => on.onSelect(node)}
      onDoubleClick={() => (node.kind === 'object' ? on.onOpen(node) : on.onToggle(node))}
      onContextMenu={(e) => on.onMenu(node, e)}
      onDragStart={draggable ? () => on.onDragStart(node) : undefined}
      onDragEnd={on.onDragEnd}
      onDragOver={(e) => on.onDragOver(node, e)}
      onDragLeave={on.onDragLeave}
      onDrop={(e) => on.onDrop(node, e)}
    >
      <span
        className="chev"
        onClick={(e) => {
          if (!row.expandable) return;
          e.stopPropagation();
          on.onSelect(node);
          on.onToggle(node);
        }}
      >
        {row.expandable && (
          <span className={`chev-icon${row.expanded ? ' open' : ''}`}>
            <ChevronIcon />
          </span>
        )}
      </span>
      {node.kind === 'connection' ? (
        <>
          {node.engine && <span className={`chip chip-${node.engine}`}>{node.engine}</span>}
          {node.envTag && <span className={`env-dot env-${node.envTag}`} title={`Environment: ${node.envTag}`} />}
        </>
      ) : (
        <NodeIcon node={node} />
      )}
      <span className={`label${node.envTag === 'production' ? ' env-production' : ''}`}>{node.label}</span>
      {node.ssh && <span className="ssh-tag">SSH</span>}
      {node.loading && <span className="spinner" aria-hidden="true" />}
      {node.meta && <span className="meta">{node.meta}</span>}
    </div>
  );
}

export function ConnectionTree() {
  const nodes = useTree((s) => s.nodes);
  const childIds = useTree((s) => s.childIds);
  const expanded = useTree((s) => s.expanded);
  const filter = useTree((s) => s.filter);
  const selectedId = useTree((s) => s.selectedId);
  const rows = useMemo(() => computeRows({ nodes, childIds, expanded, filter }), [nodes, childIds, expanded, filter]);

  const scrollRef = useRef<HTMLDivElement>(null);
  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => ROW_HEIGHT,
    overscan: 10,
    getItemKey: (index) => rows[index]?.key ?? index,
  });

  const [menu, setMenu] = useState<{ node: TreeNode; x: number; y: number } | null>(null);
  const [dropId, setDropId] = useState<string | null>(null);
  const dragging = useRef<string | null>(null);

  const select = useCallback((node: TreeNode) => {
    useTree.getState().select(node.id);
    scrollRef.current?.focus({ preventScroll: true });
  }, []);

  // Keep the selected row in view when the keyboard moves it.
  useEffect(() => {
    const index = rows.findIndex((r) => r.type === 'node' && r.key === selectedId);
    if (index >= 0) virtualizer.scrollToIndex(index, { align: 'auto' });
  }, [selectedId, rows, virtualizer]);

  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    // Keys on a button inside a row (Retry) belong to that button.
    if ((e.target as HTMLElement).closest('button')) return;
    const tree = useTree.getState();
    const nodeRows = rows.filter((r) => r.type === 'node');
    const at = nodeRows.findIndex((r) => r.key === selectedId);
    const current = nodeRows[at];
    const go = (r?: TreeRow) => r && tree.select(r.key);
    switch (e.key) {
      case 'ArrowDown':
        go(nodeRows[at < 0 ? 0 : Math.min(at + 1, nodeRows.length - 1)]);
        break;
      case 'ArrowUp':
        go(nodeRows[Math.max(at - 1, 0)]);
        break;
      case 'Home':
        go(nodeRows[0]);
        break;
      case 'End':
        go(nodeRows[nodeRows.length - 1]);
        break;
      case 'ArrowRight':
        if (!current?.expandable) return;
        if (!current.expanded) tree.expand(current.key);
        else go(nodeRows[at + 1]?.level === current.level + 1 ? nodeRows[at + 1] : undefined);
        break;
      case 'ArrowLeft':
        if (!current) return;
        if (current.expanded && !filter) tree.collapse(current.key);
        else go(nodeRows.find((r) => r.key === current.node.parentId));
        break;
      case 'Enter':
        if (!current) return;
        if (current.node.kind === 'object') openObjectTab(current.node);
        else tree.toggle(current.key);
        break;
      case 'F5':
        if (!current) return;
        void tree.refresh(current.key);
        break;
      default:
        return;
    }
    e.preventDefault();
  };

  const canDrop = (target: TreeNode | null) => {
    const drag = dragging.current;
    if (!drag || drag === target?.id) return false;
    // Rows other than groups and connections are no drop targets.
    return !target || target.kind === 'group' || target.kind === 'connection';
  };

  return (
    <>
    <div
      ref={scrollRef}
      className="tree"
      role="tree"
      aria-label="Connections"
      aria-activedescendant={selectedId ? domId(selectedId) : undefined}
      tabIndex={0}
      onKeyDown={onKeyDown}
      onDragOver={(e) => canDrop(null) && e.preventDefault()}
      onDrop={(e) => {
        const drag = dragging.current;
        if (!drag) return;
        e.preventDefault();
        dragging.current = null;
        void useTree.getState().move(drag, null);
      }}
    >
      <div style={{ height: virtualizer.getTotalSize(), position: 'relative' }}>
        {virtualizer.getVirtualItems().map((item) => {
          const row = rows[item.index];
          if (!row) return null;
          return (
            <TreeRowView
              key={row.key}
              row={row}
              selected={row.type === 'node' && row.key === selectedId}
              dropTarget={dropId === row.key}
              style={{ position: 'absolute', top: 0, left: 0, right: 0, height: ROW_HEIGHT, transform: `translateY(${item.start}px)` }}
              onSelect={select}
              onToggle={(node) => useTree.getState().toggle(node.id)}
              onOpen={openObjectTab}
              onMenu={(node, e) => {
                e.preventDefault();
                select(node);
                setMenu({ node, x: e.clientX, y: e.clientY });
              }}
              onDragStart={(node) => (dragging.current = node.id)}
              onDragEnd={() => {
                dragging.current = null;
                setDropId(null);
              }}
              onDragOver={(node, e) => {
                // Not a target: swallow the event so the root drop zone does not take it.
                e.stopPropagation();
                if (!canDrop(node)) return;
                e.preventDefault();
                setDropId(node.id);
              }}
              onDragLeave={() => setDropId(null)}
              onDrop={(node, e) => {
                const drag = dragging.current;
                if (!drag || !canDrop(node)) return;
                e.preventDefault();
                e.stopPropagation();
                dragging.current = null;
                setDropId(null);
                void useTree.getState().move(drag, node.id);
              }}
            />
          );
        })}
      </div>
    </div>
      {menu && <TreeContextMenu node={menu.node} x={menu.x} y={menu.y} onClose={() => setMenu(null)} />}
    </>
  );
}
