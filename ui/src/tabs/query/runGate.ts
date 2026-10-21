import type { ServerInfo } from '../../bindings';
import { useConnections } from '../../store/connections';
import { useExporting } from '../../store/queryExport';
import { canRun, emptyRun, useQueryRun } from '../../store/queryRun';
import { useStarting } from '../../store/runQuery';
import { analyzeAvailable } from './explainTree';

export const ANALYZE_NEEDS = 'Explain Analyze needs MySQL 8.0.18 or later';

export interface RunGate {
  /** The tab can start a run (or an Explain) now. */
  runnable: boolean;
  /** Why it cannot, for the disabled controls' title. */
  why: string | undefined;
  /** The server supports Explain Analyze. */
  analyzeOk: boolean;
  info: ServerInfo | undefined;
}

/**
 * Whether a query tab can run now, and why not: shared by the toolbar's Run / Explain controls
 * and the Explain panel's buttons. A tab without a (still stored) connection cannot run; a run
 * or an export of its result keeps it busy.
 */
export function useRunGate(tabId: string, connectionId: string | null): RunGate {
  const stored = useConnections((s) => (connectionId ? s.summaries.has(connectionId) : false));
  const info = useConnections((s) => (connectionId ? s.info.get(connectionId) : undefined));
  const free = useQueryRun((s) => canRun(s.runs.get(tabId) ?? emptyRun));
  const starting = useStarting((s) => s.ids.has(tabId));
  const exporting = useExporting((s) => s.runs.has(tabId));
  const runnable = !!connectionId && stored && free && !starting && !exporting;
  const why = runnable
    ? undefined
    : !connectionId
      ? 'Choose a connection to run queries'
      : !stored
        ? 'The connection of this tab no longer exists'
        : exporting
          ? 'An export is reading this tab’s result — stop it first'
          : 'A statement is running — stop it first';
  return { runnable, why, analyzeOk: analyzeAvailable(info), info };
}
