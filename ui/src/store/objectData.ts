import { create } from 'zustand';
import { api, isCancelled, notOpenError, toAppError } from '../api';
import { promptHandles, reopenConnection } from './reopen';
import type { AppError, ObjectRef, TableDetail } from '../bindings';
import type { ObjectTabRef as Tab } from './tabs';
import { useTabs } from './tabs';
import { objectNodeKey } from './treeModel';

export type Resource = 'detail' | 'ddl';
export type SubTab = 'data' | 'info' | 'ddl' | 'structure';

export type Loadable<T> =
  | { status: 'loading' }
  | { status: 'ok'; data: T }
  | { status: 'error'; error: AppError };

export interface ObjectData {
  sub?: SubTab;
  detail?: Loadable<TableDetail>;
  ddl?: Loadable<string>;
}

interface ObjectDataState {
  /** Per tab id; lives as long as the tab is open. */
  byTab: Record<string, ObjectData>;
  setSub(tabId: string, sub: SubTab): void;
  /** Fetches `resource` unless it is loaded or loading (or `force`). */
  load(tab: Tab, resource: Resource, force?: boolean): Promise<void>;
  /** Drops the backend cache of the object, then re-fetches `active` and forgets the rest. */
  refresh(tab: Tab, active: Resource): Promise<void>;
  /** Opens the tab's connection again (`not_open` / `connection_lost`), then re-fetches. */
  reconnect(tab: Tab, resource: Resource): Promise<void>;
}

// Monotonic, so a request of a closed (and reopened) tab never matches a newer token.
let counter = 0;
const tokens = new Map<string, number>();
const tokenKey = (tabId: string, resource: Resource) => `${tabId}\u0000${resource}`;

function fetchResource(connectionId: string, obj: ObjectRef, resource: Resource) {
  return resource === 'detail' ? api.tableDetail(connectionId, obj) : api.objectDdl(connectionId, obj);
}

export const useObjectData = create<ObjectDataState>()((set, get) => {
  const patch = (tabId: string, change: Partial<ObjectData>) =>
    set((s) => ({ byTab: { ...s.byTab, [tabId]: { ...s.byTab[tabId], ...change } } }));

  return {
    byTab: {},
    setSub: (tabId, sub) => patch(tabId, { sub }),
    async load(tab, resource, force = false) {
      const current = get().byTab[tab.id]?.[resource];
      if (!force && current && current.status !== 'error') return;
      const token = ++counter;
      const key = tokenKey(tab.id, resource);
      tokens.set(key, token);
      patch(tab.id, { [resource]: { status: 'loading' } });
      // Ignore the answer if the tab was closed or a newer request superseded this one.
      const latest = () => tokens.get(key) === token && useTabs.getState().tabs.some((t) => t.id === tab.id);
      try {
        const data = await fetchResource(tab.connectionId, tab.objectRef, resource);
        if (latest()) patch(tab.id, { [resource]: { status: 'ok', data } });
      } catch (e) {
        if (!latest()) return;
        const error = toAppError(e);
        // A cancelled call is not an error: forget it (the next visit fetches again).
        patch(tab.id, { [resource]: isCancelled(error) ? undefined : { status: 'error', error } });
      }
    },
    async reconnect(tab, resource) {
      patch(tab.id, { [resource]: { status: 'loading' } });
      const error = await reopenConnection(tab.connectionId);
      if (!useTabs.getState().tabs.some((t) => t.id === tab.id)) return;
      if (!error) {
        await get().load(tab, resource, true);
      } else {
        // The password / host-key prompt finishes the open; the tab keeps its Reconnect.
        const shown = promptHandles(error) ? notOpenError() : error;
        patch(tab.id, { [resource]: { status: 'error', error: shown } });
      }
    },
    async refresh(tab, active) {
      const other: Resource = active === 'detail' ? 'ddl' : 'detail';
      // Forget the other result (and orphan its in-flight request); it is fetched again on demand.
      tokens.delete(tokenKey(tab.id, other));
      patch(tab.id, { [other]: undefined, [active]: { status: 'loading' } });
      try {
        await api.refreshNode(tab.connectionId, objectNodeKey(tab.objectRef));
      } catch {
        // The backend cache is only an optimisation; the fetch below reports real failures.
      }
      if (!useTabs.getState().tabs.some((t) => t.id === tab.id)) return;
      await get().load(tab, active, true);
    },
  };
});

// Forget the data (and ignore in-flight answers) of tabs that were closed.
useTabs.subscribe((state) => {
  const open = new Set(state.tabs.map((t) => t.id));
  const { byTab } = useObjectData.getState();
  const gone = Object.keys(byTab).filter((id) => !open.has(id));
  if (gone.length === 0) return;
  for (const id of gone) {
    tokens.delete(tokenKey(id, 'detail'));
    tokens.delete(tokenKey(id, 'ddl'));
  }
  useObjectData.setState({ byTab: Object.fromEntries(Object.entries(byTab).filter(([id]) => open.has(id))) });
});

/** DDL invalidation also orphans responses already in flight. */
export function invalidateObjectData(tabId: string): void {
  tokens.delete(tokenKey(tabId, 'detail'));
  tokens.delete(tokenKey(tabId, 'ddl'));
  useObjectData.setState((s) => ({ byTab: Object.fromEntries(Object.entries(s.byTab).filter(([id]) => id !== tabId)) }));
}
