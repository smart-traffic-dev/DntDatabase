// Commit mode (Auto | Manual) and transaction facts of each query tab (spec §4.2d): the mode the
// toolbar toggles, the open transaction's age and size for the status bar, and a Manual
// transaction lost with its connection, which blocks runs until a Rollback acknowledges it.
import { create } from 'zustand';
import { api, toAppError } from '../api';
import type { AppError, TabTxInfo, TxMode } from '../bindings';
import { useQueryRun } from './queryRun';
import { useQueryDocs } from './queryTabs';

export interface TabTx {
  mode: TxMode;
  /** Last answer of `query_tx_info` (kept while a run makes it answer `busy`). */
  info: TabTxInfo | null;
  /** A Manual transaction lost with the connection, waiting for a Rollback. */
  lost: { outcomeUnknown: boolean } | null;
  /** `since_ms` of the transaction the idle warning toast was already shown for. */
  warnedFor: number | null;
}

const initial: TabTx = { mode: 'Auto', info: null, lost: null, warnedFor: null };

interface TxModeState {
  tabs: Map<string, TabTx>;
  patch(tabId: string, p: Partial<TabTx>): void;
  reset(): void;
}

export const useTxMode = create<TxModeState>()((set) => ({
  tabs: new Map(),
  patch: (tabId, p) =>
    set((s) => ({ tabs: new Map(s.tabs).set(tabId, { ...(s.tabs.get(tabId) ?? initial), ...p }) })),
  reset: () => set({ tabs: new Map() }),
}));

/** The tab's facts (the defaults for a tab that never changed mode). */
export const txOf = (tabs: Map<string, TabTx>, tabId: string): TabTx => tabs.get(tabId) ?? initial;

/**
 * `query_tx_mode`. Resolves to the error when the backend refuses (a transaction open, or a lost one
 * not yet acknowledged): the mode stays what it was and the caller shows the error.
 */
export async function setTxMode(tabId: string, mode: TxMode): Promise<AppError | null> {
  try {
    const state = await api.queryTxMode(tabId, mode);
    useTxMode.getState().patch(tabId, { mode, ...(mode === 'Auto' ? { lost: null } : {}) });
    useQueryRun.getState().setTxState(tabId, state);
    return null;
  } catch (e) {
    return toAppError(e);
  }
}

/** Reads the tab's transaction facts; a `busy` answer (the tab runs) or any failure keeps the last value. */
export async function refreshTxInfo(tabId: string): Promise<void> {
  try {
    const info = await api.queryTxInfo(tabId);
    useTxMode.getState().patch(tabId, { info });
  } catch {
    // Busy while a run is active: asked again after it.
  }
}

/**
 * A run failure that leaves a Manual transaction to acknowledge: the connection was lost while one was
 * open (not retryable) or the outcome of the last statement is unknown. Marks the tab so the status
 * bar asks for a Rollback. Auto tabs have nothing to acknowledge.
 */
export function noteRunFailure(tabId: string, error: AppError): void {
  if (error.kind !== 'connection_lost' || error.retryable) return;
  if (useTxMode.getState().tabs.get(tabId)?.mode !== 'Manual') return;
  useTxMode.getState().patch(tabId, { lost: { outcomeUnknown: error.outcome_unknown } });
}

// A closed tab drops its facts.
const unsubscribeDocs = useQueryDocs.subscribe((s, prev) => {
  if (s.docs === prev.docs) return;
  const gone = [...prev.docs.keys()].filter((id) => !s.docs.has(id));
  if (gone.length === 0) return;
  useTxMode.setState((t) => {
    const tabs = new Map(t.tabs);
    for (const id of gone) tabs.delete(id);
    return { tabs };
  });
});

import.meta.hot?.dispose(unsubscribeDocs);
