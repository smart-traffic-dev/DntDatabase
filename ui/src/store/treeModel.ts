import type { AppError, ObjectKind, ObjectRef, Scope, ServerVariant } from '../bindings';

export type Engine = 'MY' | 'PG' | 'MA';

export interface TreeNode {
  id: string;
  parentId?: string;
  kind: 'group' | 'connection' | 'database' | 'schema' | 'folder' | 'object';
  label: string;
  meta?: string;
  engine?: Engine;
  envTag?: 'production' | 'staging' | 'local';
  ssh?: boolean;
  loading?: boolean;
  error?: AppError;
  objectRef?: ObjectRef;
  folderKind?: ObjectKind;
  /** Database (and schema) a database / schema / folder node lists. */
  scope?: Scope;
}

export interface TreeRow {
  /** Unique row key; placeholder rows are `<node id>#loading` / `#error`. */
  key: string;
  type: 'node' | 'loading' | 'error';
  /** The node of the row (the owner of a placeholder row). */
  node: TreeNode;
  /** 1-based depth, as `aria-level`. */
  level: number;
  expandable: boolean;
  expanded: boolean;
  /** While filtering: how many direct children match (the loaded ones; meta counts all). */
  matchCount?: number;
}

export interface RowSource {
  nodes: Map<string, TreeNode>;
  childIds: Map<string, string[]>;
  expanded: Set<string>;
  filter: string;
}

/** Key of the children list of the top-level nodes. */
export const ROOT = '';

export const connNodeId = (connectionId: string) => `conn:${connectionId}`;
export const groupNodeId = (groupId: string) => `group:${groupId}`;

/** UUID of the connection a node id belongs to (ids start with `conn:<uuid>`). */
export function connectionIdOf(nodeId: string): string | undefined {
  const first = nodeId.split('/')[0] ?? '';
  return first.startsWith('conn:') ? first.slice('conn:'.length) : undefined;
}

/** A name as the value part of a node-id segment: `/` and `:` never appear raw. */
export const seg = (name: string) => encodeURIComponent(name);

/**
 * The backend `NodeKey` of a node: its id path without the `conn:<uuid>` segment, with the
 * value of every segment decoded (backend keys carry raw names).
 */
export const nodeKey = (nodeId: string): string[] =>
  nodeId
    .split('/')
    .slice(1)
    .map((segment) => {
      // `obj:<Kind>:<value>` has two prefixes, `db:` / `schema:` / `folder:` one.
      const match = /^(obj:[^:]*:|[^:]+:)(.*)$/.exec(segment);
      return match ? `${match[1]}${decodeURIComponent(match[2] ?? '')}` : segment;
    });

/** The `refresh_node` key of an object (its tree path; `detail` / `ddl` are dropped with it). */
export function objectNodeKey(o: ObjectRef): string[] {
  const id = [
    'conn:-',
    `db:${seg(o.database)}`,
    ...(o.schema === null ? [] : [`schema:${seg(o.schema)}`]),
    `folder:${o.kind}`,
    `obj:${o.kind}:${seg(o.name)}`,
  ].join('/');
  return nodeKey(id);
}

export function engineOf(variant: ServerVariant): Engine {
  switch (variant) {
    case 'Postgres':
      return 'PG';
    case 'MariaDb':
      return 'MA';
    default:
      return 'MY';
  }
}

/** Object folders of a database (MySQL family) or schema (PostgreSQL), spec section 6. */
export function folderKindsOf(variant: ServerVariant): ObjectKind[] {
  switch (variant) {
    case 'Postgres':
      return ['Table', 'View', 'MaterializedView', 'Function', 'Procedure', 'Sequence', 'Type'];
    case 'MariaDb':
      return ['Table', 'View', 'Function', 'Procedure', 'Trigger', 'Event', 'Sequence'];
    default:
      return ['Table', 'View', 'Function', 'Procedure', 'Trigger', 'Event'];
  }
}

const FOLDER_LABELS: Record<ObjectKind, string> = {
  Table: 'Tables',
  View: 'Views',
  MaterializedView: 'Materialized views',
  Function: 'Functions',
  Procedure: 'Procedures',
  Trigger: 'Triggers',
  Event: 'Events',
  Sequence: 'Sequences',
  Type: 'Types',
};

export const folderLabel = (kind: ObjectKind) => FOLDER_LABELS[kind];

export function isExpandable(node: TreeNode): boolean {
  return node.kind !== 'object';
}

/** The rows to render: expanded nodes only, or only the matches and their parents when filtering. */
export function computeRows(s: RowSource): TreeRow[] {
  const query = s.filter.trim().toLowerCase();
  const rows: TreeRow[] = [];

  // Whether a node, or something below it, matches the filter (memoized per call).
  const memo = new Map<string, boolean>();
  const subtreeMatches = (id: string): boolean => {
    const known = memo.get(id);
    if (known !== undefined) return known;
    const node = s.nodes.get(id);
    let result = !!node && node.label.toLowerCase().includes(query);
    if (!result) {
      for (const child of s.childIds.get(id) ?? []) {
        if (subtreeMatches(child)) {
          result = true;
          break;
        }
      }
    }
    memo.set(id, result);
    return result;
  };
  const hasMatchingChild = (id: string) =>
    (s.childIds.get(id) ?? []).some((child) => subtreeMatches(child));

  const walk = (parentKey: string, level: number) => {
    for (const id of s.childIds.get(parentKey) ?? []) {
      const node = s.nodes.get(id);
      if (!node) continue;
      if (query && !subtreeMatches(id)) continue;
      const expanded = query ? hasMatchingChild(id) : s.expanded.has(id);
      const matchCount = query ? (s.childIds.get(id) ?? []).filter(subtreeMatches).length : undefined;
      rows.push({ key: id, type: 'node', node, level, expandable: isExpandable(node), expanded, ...(matchCount === undefined ? {} : { matchCount }) });
      if (!expanded) continue;
      walk(id, level + 1);
      if (query) continue;
      if (node.loading) {
        rows.push({ key: `${id}#loading`, type: 'loading', node, level: level + 1, expandable: false, expanded: false });
      } else if (node.error) {
        rows.push({ key: `${id}#error`, type: 'error', node, level: level + 1, expandable: false, expanded: false });
      }
    }
  };
  walk(ROOT, 1);
  return rows;
}
