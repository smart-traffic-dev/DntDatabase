import { create } from 'zustand';
import type { ConnectionSummary, Group, ServerInfo } from '../bindings';

interface ConnectionsState {
  groups: Group[];
  /** Stored connections by id. */
  summaries: Map<string, ConnectionSummary>;
  /** Server info of the connections that are open. */
  info: Map<string, ServerInfo>;
  setStored(groups: Group[], connections: ConnectionSummary[]): void;
  setInfo(id: string, info: ServerInfo): void;
  clearInfo(id: string): void;
  reset(): void;
}

export const useConnections = create<ConnectionsState>()((set) => ({
  groups: [],
  summaries: new Map(),
  info: new Map(),
  setStored: (groups, connections) =>
    set({ groups, summaries: new Map(connections.map((c) => [c.config.id, c])) }),
  setInfo: (id, info) => set((s) => ({ info: new Map(s.info).set(id, info) })),
  clearInfo: (id) =>
    set((s) => {
      const info = new Map(s.info);
      info.delete(id);
      return { info };
    }),
  reset: () => set({ groups: [], summaries: new Map(), info: new Map() }),
}));
