import { create } from 'zustand';
import { api, isHostKeyMismatch, isHostKeyUnknown, isPasswordRequired, toAppError } from '../api';
import type { AppError, ConnectionSummary, Group, ServerInfo } from '../bindings';
import { useConnections } from './connections';
import { useDialogs } from './dialogs';
import {
  ROOT,
  computeRows,
  connNodeId,
  connectionIdOf,
  engineOf,
  folderKindsOf,
  folderLabel,
  groupNodeId,
  nodeKey,
  seg,
  type TreeNode,
  type TreeRow,
} from './treeModel';

export type { Engine, TreeNode, TreeRow } from './treeModel';

interface TreeState {
  nodes: Map<string, TreeNode>;
  /** Ordered child ids per parent id (`ROOT` for the top level); present once loaded. */
  childIds: Map<string, string[]>;
  expanded: Set<string>;
  filter: string;
  showSystem: boolean;
  selectedId?: string;
  /** Last failure of a tree-wide action (loading or moving connections). */
  error?: AppError;

  toggle(id: string): void;
  expand(id: string): void;
  collapse(id: string): void;
  select(id: string | undefined): void;
  setFilter(filter: string): void;
  setShowSystem(show: boolean): void;
  /** Invalidates the backend cache of the node (`refresh_node`) and reloads what is shown. */
  refresh(id: string): Promise<void>;
  loadChildren(id: string): Promise<void>;
  /** Loads groups and connections from the store (`list_tree`). */
  reloadStored(): Promise<void>;
  closeConnection(connectionId: string): Promise<void>;
  /** Shows a connection as closed without calling the backend (its runtime is already gone). */
  forgetConnection(connectionId: string): void;
  duplicateConnection(connectionId: string): Promise<void>;
  /** Drag and drop: moves a group or connection into / next to `targetId` (`null` = the root). */
  move(dragId: string, targetId: string | null): Promise<void>;
  clearError(): void;
  visibleRows(): TreeRow[];
  reset(): void;
}

/** Mutable copy of the tree maps, committed with one `set`. */
interface Draft {
  nodes: Map<string, TreeNode>;
  childIds: Map<string, string[]>;
  expanded: Set<string>;
}

const initial = () => ({
  nodes: new Map<string, TreeNode>(),
  childIds: new Map<string, string[]>(),
  expanded: new Set<string>(),
  filter: '',
  showSystem: false,
  selectedId: undefined,
  error: undefined,
});

function dropChildren(d: Draft, id: string) {
  for (const child of d.childIds.get(id) ?? []) dropNode(d, child);
  d.childIds.delete(id);
}

function dropNode(d: Draft, id: string) {
  dropChildren(d, id);
  d.nodes.delete(id);
  d.expanded.delete(id);
}

const uuidOf = (nodeId: string) => nodeId.slice(nodeId.indexOf(':') + 1);

const versionShort = (info: ServerInfo) => `${info.version[0]}.${info.version[1]}`;

/** Newest load request per node: a slower, older one must not overwrite a newer result. */
const requests = new Map<string, number>();

/** Makes every in-flight load of `id` and its descendants stale. */
function invalidate(id: string) {
  for (const key of [...requests.keys()]) {
    if (key === id || key.startsWith(`${id}/`)) requests.set(key, (requests.get(key) ?? 0) + 1);
  }
}

interface Loaded {
  children: TreeNode[];
  parentPatch?: Partial<TreeNode>;
}

export const useTree = create<TreeState>()((set, get) => {
  const draft = (): Draft => {
    const s = get();
    return { nodes: new Map(s.nodes), childIds: new Map(s.childIds), expanded: new Set(s.expanded) };
  };

  /** Commits a draft; the selection goes away with the node it points at. */
  const commit = (d: Draft) => {
    const selected = get().selectedId;
    set(selected && !d.nodes.has(selected) ? { ...d, selectedId: undefined } : d);
  };

  const patch = (id: string, change: Partial<TreeNode>) =>
    set((s) => {
      const node = s.nodes.get(id);
      if (!node) return s;
      return { nodes: new Map(s.nodes).set(id, { ...node, ...change }) };
    });

  /** Replaces the children of `parentId`; children that stay keep their own subtree. */
  const setChildren = (parentId: string, children: TreeNode[], parentPatch: Partial<TreeNode> = {}) => {
    const d = draft();
    const keep = new Set(children.map((c) => c.id));
    for (const old of d.childIds.get(parentId) ?? []) if (!keep.has(old)) dropNode(d, old);
    for (const child of children) {
      const prev = d.nodes.get(child.id);
      d.nodes.set(child.id, prev ? { ...prev, ...child, meta: child.meta ?? prev.meta } : child);
    }
    d.childIds.set(parentId, children.map((c) => c.id));
    const parent = d.nodes.get(parentId);
    if (parent) {
      d.nodes.set(parentId, { ...parent, loading: false, error: undefined, ...parentPatch });
    }
    commit(d);
  };

  const connectionNode = (c: ConnectionSummary, previous?: TreeNode): TreeNode => {
    const cfg = c.config;
    const info = useConnections.getState().info.get(cfg.id);
    return {
      ...previous,
      id: connNodeId(cfg.id),
      parentId: cfg.group_id ? groupNodeId(cfg.group_id) : undefined,
      kind: 'connection',
      label: cfg.name,
      engine: info ? engineOf(info.variant) : cfg.kind === 'postgres' ? 'PG' : 'MY',
      meta: info ? versionShort(info) : undefined,
      envTag: cfg.env_tag ?? undefined,
      ssh: !!cfg.ssh,
    };
  };

  const buildStored = (groups: Group[], connections: ConnectionSummary[]) => {
    const d = draft();
    const wanted = new Set([
      ...groups.map((g) => groupNodeId(g.id)),
      ...connections.map((c) => connNodeId(c.config.id)),
    ]);
    for (const [id, node] of d.nodes) {
      if ((node.kind === 'group' || node.kind === 'connection') && !wanted.has(id)) dropNode(d, id);
    }
    const children = new Map<string, string[]>();
    const add = (parent: string, id: string) => children.set(parent, [...(children.get(parent) ?? []), id]);
    const bySort = <T extends { sort_order: number }>(a: T, b: T) => a.sort_order - b.sort_order;
    for (const g of [...groups].sort(bySort)) {
      const id = groupNodeId(g.id);
      const parentId = g.parent_id ? groupNodeId(g.parent_id) : undefined;
      d.nodes.set(id, { id, parentId, kind: 'group', label: g.name });
      add(parentId ?? ROOT, id);
    }
    for (const c of [...connections].sort((a, b) => bySort(a.config, b.config))) {
      const node = connectionNode(c, d.nodes.get(connNodeId(c.config.id)));
      d.nodes.set(node.id, node);
      add(node.parentId ?? ROOT, node.id);
    }
    for (const id of [ROOT, ...groups.map((g) => groupNodeId(g.id))]) {
      d.childIds.set(id, children.get(id) ?? []);
    }
    commit(d);
  };

  /** Fetches the children of a node; the caller applies them only if the request is still current. */
  const listChildren = async (
    node: TreeNode,
    connectionId: string,
    latest: () => boolean,
  ): Promise<Loaded | null> => {
    const id = node.id;
    if (node.kind === 'connection') {
      let info = useConnections.getState().info.get(connectionId);
      if (!info) {
        info = (await api.openConnection(connectionId, null)).info;
        // Closed or superseded meanwhile: record nothing, list nothing.
        if (!latest()) return null;
        useConnections.getState().setInfo(connectionId, info);
        patch(id, { engine: engineOf(info.variant), meta: versionShort(info) });
      }
      const dbs = await api.listDatabases(connectionId, get().showSystem);
      return {
        children: dbs.map((db) => ({
          id: `${id}/db:${seg(db.name)}`,
          parentId: id,
          kind: 'database',
          label: db.name,
          scope: { database: db.name, schema: null },
        })),
      };
    }
    const variant = useConnections.getState().info.get(connectionId)?.variant;
    if (!variant) throw new Error('The connection is not open.');
    const scope = node.scope;
    if (!scope) return null;
    if (node.kind === 'database' && variant === 'Postgres') {
      const schemas = await api.listSchemas(connectionId, scope.database, get().showSystem);
      return {
        children: schemas.map((s) => ({
          id: `${id}/schema:${seg(s.name)}`,
          parentId: id,
          kind: 'schema',
          label: s.name,
          scope: { database: scope.database, schema: s.name },
        })),
      };
    }
    if (node.kind === 'database' || node.kind === 'schema') {
      return {
        children: folderKindsOf(variant).map((kind) => ({
          id: `${id}/folder:${kind}`,
          parentId: id,
          kind: 'folder',
          label: folderLabel(kind),
          folderKind: kind,
          scope,
        })),
      };
    }
    if (node.kind === 'folder' && node.folderKind) {
      const kind = node.folderKind;
      const objects = await api.listObjects(connectionId, scope, kind);
      return {
        children: objects.map((o) => ({
          id: `${id}/obj:${kind}:${seg(o.name)}`,
          parentId: id,
          kind: 'object',
          label: o.name,
          objectRef: { database: scope.database, schema: scope.schema, name: o.name, kind },
        })),
        parentPatch: { meta: String(objects.length) },
      };
    }
    return null;
  };

  const loadChildren = async (id: string) => {
    const node = get().nodes.get(id);
    const connectionId = connectionIdOf(id);
    if (!node || !connectionId || node.kind === 'group' || node.kind === 'object') return;
    const token = (requests.get(id) ?? 0) + 1;
    requests.set(id, token);
    const latest = () => requests.get(id) === token && get().nodes.has(id);
    patch(id, { loading: true, error: undefined });
    try {
      const loaded = await listChildren(node, connectionId, latest);
      // A newer request, a close, or a removed node supersedes this answer.
      if (!latest()) return;
      if (!loaded) {
        patch(id, { loading: false });
        return;
      }
      setChildren(id, loaded.children, loaded.parentPatch);
    } catch (e) {
      if (!latest()) return;
      const error = toAppError(e);
      const prompt = isPasswordRequired(error)
        ? 'password'
        : isHostKeyUnknown(error) || isHostKeyMismatch(error)
          ? 'hostKey'
          : null;
      if (node.kind === 'connection' && prompt) {
        // Task 18's dialogs answer the prompt, then expand the connection again.
        patch(id, { loading: false });
        get().collapse(id);
        useDialogs.getState().showPrompt({ kind: prompt, connectionId, error });
        return;
      }
      patch(id, { loading: false, error });
    }
  };

  /** Reloads what is expanded below `id` (top down); collapsed branches reload when opened. */
  const reloadSubtree = async (id: string) => {
    const d = draft();
    const prune = (parent: string) => {
      for (const child of d.childIds.get(parent) ?? []) {
        if (d.expanded.has(child)) prune(child);
        else dropChildren(d, child);
      }
    };
    if (!d.expanded.has(id)) {
      dropChildren(d, id);
      commit(d);
      return;
    }
    prune(id);
    commit(d);
    const targets = [...get().expanded]
      .filter((e) => e === id || e.startsWith(`${id}/`))
      .sort((a, b) => a.length - b.length);
    for (const target of targets) {
      if (get().nodes.has(target)) await loadChildren(target);
    }
  };

  return {
    ...initial(),

    toggle: (id) => {
      if (get().expanded.has(id)) get().collapse(id);
      else get().expand(id);
    },
    expand: (id) => {
      const node = get().nodes.get(id);
      if (!node || node.kind === 'object') return;
      set((s) => ({ expanded: new Set(s.expanded).add(id) }));
      if (node.kind !== 'group' && !get().childIds.has(id) && !node.loading) void loadChildren(id);
    },
    collapse: (id) =>
      set((s) => {
        const expanded = new Set(s.expanded);
        expanded.delete(id);
        return { expanded };
      }),
    select: (selectedId) => set({ selectedId }),
    setFilter: (filter) => set({ filter }),
    setShowSystem: (showSystem) => {
      set({ showSystem });
      for (const [id, node] of get().nodes) {
        if (node.kind === 'connection' && get().childIds.has(id)) void reloadSubtree(id);
      }
    },

    refresh: async (id) => {
      const node = get().nodes.get(id);
      const connectionId = connectionIdOf(id);
      if (!node) return;
      if (node.kind === 'group' || !connectionId || !useConnections.getState().info.has(connectionId)) {
        await get().reloadStored();
        return;
      }
      try {
        await api.refreshNode(connectionId, nodeKey(id));
      } catch (e) {
        // The node may be collapsed or a leaf: report through the sidebar banner.
        set({ error: toAppError(e) });
        return;
      }
      await reloadSubtree(node.kind === 'object' && node.parentId ? node.parentId : id);
    },
    loadChildren,

    reloadStored: async () => {
      try {
        const tree = await api.listTree();
        useConnections.getState().setStored(tree.groups, tree.connections);
        buildStored(tree.groups, tree.connections);
        set({ error: undefined });
      } catch (e) {
        set({ error: toAppError(e) });
      }
    },

    closeConnection: async (connectionId) => {
      const id = connNodeId(connectionId);
      // Loads still in flight must not bring the connection's children or info back.
      invalidate(id);
      try {
        await api.closeConnection(connectionId);
      } catch (e) {
        set({ error: toAppError(e) });
        return;
      }
      get().forgetConnection(connectionId);
    },

    forgetConnection: (connectionId) => {
      const id = connNodeId(connectionId);
      invalidate(id);
      useConnections.getState().clearInfo(connectionId);
      const d = draft();
      dropChildren(d, id);
      d.expanded.delete(id);
      const node = d.nodes.get(id);
      const stored = useConnections.getState().summaries.get(connectionId);
      if (node) {
        d.nodes.set(id, {
          ...node,
          loading: false,
          error: undefined,
          meta: undefined,
          engine: stored?.config.kind === 'postgres' ? 'PG' : 'MY',
        });
      }
      commit(d);
    },

    duplicateConnection: async (connectionId) => {
      try {
        await api.duplicateConnection(connectionId);
      } catch (e) {
        set({ error: toAppError(e) });
        return;
      }
      await get().reloadStored();
    },

    move: async (dragId, targetId) => {
      const { nodes, childIds } = get();
      const drag = nodes.get(dragId);
      const target = targetId ? nodes.get(targetId) : undefined;
      if (!drag || (drag.kind !== 'group' && drag.kind !== 'connection')) return;
      if (targetId && (!target || targetId === dragId)) return;
      // Only groups and connections are drop targets.
      if (target && target.kind !== 'group' && target.kind !== 'connection') return;

      // Into the target when it is a group, else next to it.
      const intoGroup = target?.kind === 'group';
      const parentNode = targetId ? (intoGroup ? target?.id : target?.parentId) : undefined;
      for (let p = parentNode; p; p = nodes.get(p)?.parentId) {
        if (p === dragId) return; // a group cannot go below itself
      }
      const siblings = (childIds.get(parentNode ?? ROOT) ?? []).filter(
        (id) => id !== dragId && nodes.get(id)?.kind === drag.kind,
      );
      const before = !intoGroup && target?.kind === drag.kind ? siblings.indexOf(target.id) : -1;
      const index = before >= 0 ? before : siblings.length;
      const node = drag.kind === 'group' ? ({ type: 'group', id: uuidOf(dragId) } as const) : ({ type: 'connection', id: uuidOf(dragId) } as const);
      try {
        await api.moveNode(node, parentNode ? uuidOf(parentNode) : null, index);
      } catch (e) {
        set({ error: toAppError(e) });
        return;
      }
      await get().reloadStored();
    },

    clearError: () => set({ error: undefined }),
    visibleRows: () => computeRows(get()),
    reset: () => {
      requests.clear();
      set(initial());
    },
  };
});
