import { useVirtualizer } from '@tanstack/react-virtual';
import { useCallback, useEffect, useMemo, useRef, useState, type KeyboardEvent, type MouseEvent } from 'react';
import { needsReconnect } from '../api';
import { useTree, type TreeNode, type TreeRow } from '../store/tree';
import { useConnections } from '../store/connections';
import { computeRows, connectionIdOf } from '../store/treeModel';
import { EnvTag } from './EnvTag';
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
import { useDialogs } from '../store/dialogs';
import { openNewQuery } from './newQuery';
import { openObjectTab } from './openObject';
import { ActionMenu, transferItems } from './ActionMenu';
import { TreeContextMenu } from './TreeContextMenu';
import './tree.css';

export const ROW_HEIGHT = 26;
/** An error row carries the message and its hint on two lines. */
export const ERROR_ROW_HEIGHT = 42;
const heightOf = (row: TreeRow | undefined) => (row?.type === 'error' && row.node.error?.hint ? ERROR_ROW_HEIGHT : ROW_HEIGHT);

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
  const connectionId = node.kind === 'connection' ? connectionIdOf(node.id) : undefined;
  const readOnly = useConnections((s) => (connectionId ? s.summaries.get(connectionId)?.config.read_only === true : false));
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
    // A closed or lost connection is fixed by opening it again, not by repeating the call.
    const reconnect = !!error && needsReconnect(error);
    return (
      <div role="treeitem" aria-level={row.level} aria-disabled="true" aria-selected="false" data-testid="tree-row" className="tree-row placeholder error" style={{ ...style, ...indent }}>
        <span className="chev" />
        <span className="error-body" title={error?.detail ?? undefined}>
          <span className="error-text">{error?.message}</span>
          {error?.hint && <span className="error-hint">{error.hint}</span>}
        </span>
        <button
          type="button"
          className="btn small"
          onClick={(e) => {
            e.stopPropagation();
            const tree = useTree.getState();
            void (reconnect ? tree.reconnect(node.id) : tree.loadChildren(node.id));
          }}
        >
          {reconnect ? 'Reconnect' : 'Retry'}
        </button>
      </div>
    );
  }

  const draggable = node.kind === 'group' || node.kind === 'connection';
  // While filtering only loaded children are searched: show "matching / total" for a folder.
  const countMeta =
    row.matchCount && node.meta && /^\d+$/.test(node.meta) ? `${row.matchCount} / ${node.meta}` : node.meta;
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
          {node.envTag && <EnvTag env={node.envTag} />}
        </>
      ) : (
        <NodeIcon node={node} />
      )}
      <span className={`label${node.envTag === 'production' ? ' env-production' : ''}`} title={node.label}>
        {node.label}
      </span>
      {readOnly && (
        <span className="ro-tag" title="Read-only connection">
          RO
        </span>
      )}
      {node.ssh && <span className="ssh-tag">SSH</span>}
      {node.loading && <span className="spinner" aria-hidden="true" />}
      {node.meta && <span className="meta">{countMeta}</span>}
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
    estimateSize: (index) => heightOf(rows[index]),
    overscan: 10,
    getItemKey: (index) => rows[index]?.key ?? index,
  });

  const [menu, setMenu] = useState<{ node: TreeNode; x: number; y: number } | null>(null);
  const [emptyMenu, setEmptyMenu] = useState<{ x: number; y: number } | null>(null);
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
    if (e.ctrlKey || e.metaKey) {
      if (e.altKey) return;
      // Ctrl+Enter: the object's tab on DDL. Ctrl+Q: a new query bound to the node. Other Ctrl keys are not ours.
      if (e.key === 'Enter' && current?.node.kind === 'object') openObjectTab(current.node, { ddl: true });
      else if (e.key.toLowerCase() === 'q' && current) openNewQuery(current.node);
      else return;
      e.preventDefault();
      return;
    }
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
      case 'F2': {
        // Rename = the node's existing edit / rename dialog (connections, groups).
        const connectionId = connectionIdOf(current?.key ?? '');
        const dialogs = useDialogs.getState();
        if (current?.node.kind === 'connection' && connectionId) dialogs.requestDialog({ kind: 'editConnection', id: connectionId });
        else if (current?.node.kind === 'group') dialogs.requestDialog({ kind: 'renameGroup', id: current.key.slice('group:'.length) });
        else return;
        break;
      }
      case 'F5':
        if (!current) return;
        void tree.refresh(current.key);
        break;
      default: {
        // Typing jumps to the filter: the text goes in, and the filter box takes over the typing.
        if (e.key.length !== 1 || e.altKey || (e.key === ' ' && !filter)) return;
        tree.setFilter(filter + e.key);
        document.querySelector<HTMLInputElement>('input[aria-label="Filter objects"]')?.focus();
        break;
      }
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
      onContextMenu={(e) => {
        // Rows handle their own menu (and prevent the default); this is the empty area.
        if (e.defaultPrevented) return;
        e.preventDefault();
        setEmptyMenu({ x: e.clientX, y: e.clientY });
      }}
      onDragOver={(e) => canDrop(null) && e.preventDefault()}
      onDrop={(e) => {
        const drag = dragging.current;
        if (!drag) return;
        e.preventDefault();
        dragging.current = null;
        void useTree.getState().move(drag, null);
      }}
    >
      {filter.trim() && <div className="tree-hint">Searching loaded objects</div>}
      {filter.trim() && rows.length === 0 && (
        <div className="tree-empty" role="status">No objects match '{filter.trim()}' (loaded objects only)</div>
      )}
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
              style={{ position: 'absolute', top: 0, left: 0, right: 0, height: item.size, transform: `translateY(${item.start}px)` }}
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
      {emptyMenu && (
        <ActionMenu label="Connections actions" x={emptyMenu.x} y={emptyMenu.y} items={transferItems()} onClose={() => setEmptyMenu(null)} />
      )}
      {menu && <TreeContextMenu node={menu.node} x={menu.x} y={menu.y} onClose={() => setMenu(null)} />}
    </>
  );
}
