import { useEffect } from 'react';
import type { EnvTag } from '../bindings';
import { plural } from '../lib/plural';
import { useNow } from '../lib/useNow';
import { useQueryDocs } from '../store/queryTabs';
import { useAppSettings } from '../store/appSettings';
import { useConnections } from '../store/connections';
import { useDialogs } from '../store/dialogs';
import { refreshTxInfo, txOf, useTxMode } from '../store/txMode';
import { useCursor } from '../store/cursor';
import { useGridSelection } from '../store/gridSelection';
import { useObjectData } from '../store/objectData';
import { pageRowsText, useTableData } from '../store/tableData';
import { canRun, emptyRun, lastRunText, runElapsedMs, runProgress, useQueryRun } from '../store/queryRun';
import { txAction } from '../store/runQuery';
import { useTabs } from '../store/tabs';
import { useTree } from '../store/tree';
import { connectionIdOf } from '../store/treeModel';
import { LockIcon } from './icons';
import { formatClock, formatElapsed, shortVersion } from './statusFormat';

const wholeSeconds = (ms: number) => Math.floor(ms / 1000);

const ENV_LABEL: Record<EnvTag, string> = { production: 'PRODUCTION', staging: 'STAGING', local: 'LOCAL' };

/** Last run, transaction state and run/stop progress of a query tab (spec §6.2). */
function QueryStatus({ tabId }: { tabId: string }) {
  const run = useQueryRun((s) => s.runs.get(tabId) ?? emptyRun);
  const phase = run.phase;
  const since = phase.kind === 'idle' ? null : phase.kind === 'running' ? phase.startedAt : phase.since;
  const now = useNow(since !== null, since);
  const idle = canRun(run);
  const cancelled = idle && run.stop?.kind === 'cancelled';
  const elapsed = runElapsedMs(run, now);
  const progress = runProgress(run);
  // Stopping / abandoning count from when that began, not from the run's start.
  const stopSeconds = phase.kind === 'stopping' || phase.kind === 'abandoning' ? wholeSeconds(Math.max(0, now - phase.since)) : null;
  const errors = (run.lastRun?.errors ?? 0) > 0;

  // Manual commit: the open transaction's size and age, warned about once it idles too long.
  const tx = useTxMode((s) => txOf(s.tabs, tabId));
  const warnMinutes = useAppSettings((s) => s.idleTxWarnMinutes);
  const open = run.txState === 'active';
  const info = open && tx.info?.state === 'active' ? tx.info : null;
  const txNow = useNow(open && info?.since_ms != null, info?.since_ms ?? null);
  const ageMs = info?.since_ms != null ? Math.max(0, txNow - info.since_ms) : null;
  const warn = open && ageMs !== null && ageMs >= warnMinutes * 60_000;
  const openText = info
    ? ['Transaction open', plural(info.statements, 'statement'), ...(ageMs !== null ? [formatClock(ageMs)] : [])].join(' · ')
    : null;
  useEffect(() => {
    if (run.txState !== 'idle') void useAppSettings.getState().loadIdleTxWarnMinutes();
  }, [run.txState]);
  // Ask for the facts whenever the tab is at rest and may hold a transaction.
  useEffect(() => {
    if (idle && (run.txState !== 'idle' || tx.mode === 'Manual')) void refreshTxInfo(tabId);
  }, [tabId, idle, run.txState, tx.mode, run.lastRun]);
  const txSince = info?.since_ms ?? null;
  useEffect(() => {
    if (!warn || txSince === null || useTxMode.getState().tabs.get(tabId)?.warnedFor === txSince) return;
    useTxMode.getState().patch(tabId, { warnedFor: txSince });
    useDialogs
      .getState()
      .setNotice(`A transaction has been open for more than ${plural(warnMinutes, 'minute')}. Commit or roll it back.`);
  }, [warn, txSince, tabId, warnMinutes]);
  return (
    <>
      {phase.kind === 'running' && elapsed !== null && progress && (
        <span className="status-run running" data-testid="status-running">
          <span className="spinner inline" aria-hidden="true" />
          {/* Static text only: the live region announces statement changes, not every tick. */}
          <span role="status" data-testid="status-running-live">
            Running… {progress.current}/{progress.total}
          </span>
          <span aria-hidden="true"> · {formatElapsed(wholeSeconds(elapsed) * 1000)}</span>
        </span>
      )}
      {idle && run.lastRun && (
        <span
          className={`status-last${cancelled ? ' cancelled' : errors ? ' errors' : ''}`}
          data-testid="status-last"
        >
          {cancelled ? `Cancelled by user after ${formatElapsed(run.lastRun.elapsedMs)}` : lastRunText(run.lastRun)}
        </span>
      )}
      {idle && !run.lastRun && cancelled && (
        <span className="status-last cancelled" data-testid="status-last">
          Cancelled by user
        </span>
      )}
      <span className={`status-tx tx-${run.txState}${warn ? ' warn' : ''}`} data-testid="status-tx">
        {run.txState === 'idle' && (tx.mode === 'Manual' ? 'Manual commit' : 'Auto-commit')}
        {run.txState === 'active' && (openText ?? 'In transaction')}
        {run.txState === 'aborted' && 'Transaction aborted'}
        {run.txState === 'active' && (
          <button type="button" className="status-btn" disabled={!idle} onClick={() => void txAction(tabId, 'Commit')}>
            Commit
          </button>
        )}
        {run.txState !== 'idle' && (
          <button type="button" className="status-btn" disabled={!idle} onClick={() => void txAction(tabId, 'Rollback')}>
            Rollback
          </button>
        )}
      </span>
      {tx.lost && (
        <span className="status-tx tx-aborted" data-testid="status-tx-lost" role="alert">
          {tx.lost.outcomeUnknown
            ? 'Connection lost · the last statement may have been applied'
            : 'Connection lost during a transaction'}
          <button type="button" className="status-btn" disabled={!idle} onClick={() => void txAction(tabId, 'Rollback')}>
            Rollback to continue
          </button>
        </span>
      )}
      {(phase.kind === 'stopping' || phase.kind === 'abandoning') && stopSeconds !== null && (
        <span className="status-run" data-testid="status-run">
          {phase.kind === 'abandoning'
            ? `Server not responding — replacing connection… ${stopSeconds} s`
            : `Stopping… ${stopSeconds} s`}
        </span>
      )}
    </>
  );
}

/** "Ln 12, Col 8" and "(34 selected)" of the active query tab; the only part that follows the caret. */
export function CursorStatus({ tabId }: { tabId: string }) {
  const pos = useCursor((s) => s.byTab[tabId]);
  if (!pos) return null;
  return (
    <span className="status-cursor" data-testid="status-cursor">
      Ln {pos.line}, Col {pos.col}
      {pos.selected > 0 && ` (${pos.selected} selected)`}
    </span>
  );
}

/** The grid selection and the rows on screen of an object tab's Data section; nothing on its other sections. */
export function DataStatus({ tabId }: { tabId: string }) {
  const onData = useObjectData((s) => (s.byTab[tabId]?.sub ?? 'data') === 'data');
  const estimate = useObjectData((s) => {
    const detail = s.byTab[tabId]?.detail;
    return detail?.status === 'ok' ? detail.data.row_estimate : null;
  });
  const rows = useTableData((s) => {
    const state = s.byTab[tabId];
    return state ? pageRowsText(state, estimate) : null;
  });
  const selection = useGridSelection((s) => s.byTab[tabId]);
  if (!onData || rows === null) return null;
  return (
    <span className="status-right">
      {selection && (
        <span className="status-selection" data-testid="status-data-selection">
          {selection}
        </span>
      )}
      <span className="status-rows" data-testid="status-data-rows">
        {rows}
      </span>
    </span>
  );
}

/**
 * The connection of the active tab (else of the selected tree node), its server version
 * and whether it goes through SSH; coloured by its environment tag.
 */
export function StatusBar() {
  const selectedId = useTree((s) => s.selectedId);
  const queryTabId = useTabs((s) => {
    const tab = s.tabs.find((t) => t.id === s.activeId);
    return tab?.kind === 'query' ? tab.id : undefined;
  });
  const objectTabId = useTabs((s) => {
    const tab = s.tabs.find((t) => t.id === s.activeId);
    return tab?.kind === 'object' ? tab.id : undefined;
  });
  const tabConnection = useTabs((s) => s.tabs.find((t) => t.id === s.activeId)?.connectionId);
  // The database the active tab works in: the query tab's context, or the object's database.
  const objectDb = useTabs((s) => {
    const tab = s.tabs.find((t) => t.id === s.activeId);
    return tab?.kind === 'object' ? tab.objectRef.database : null;
  });
  const queryDb = useQueryDocs((s) => (queryTabId ? s.docs.get(queryTabId)?.database ?? null : null));
  const database = queryTabId ? queryDb : objectDb;
  // A query tab without a connection falls back to the tree selection.
  const connectionId = tabConnection ?? (selectedId ? connectionIdOf(selectedId) : undefined);
  const summary = useConnections((s) => (connectionId ? s.summaries.get(connectionId) : undefined));
  const info = useConnections((s) => (connectionId ? s.info.get(connectionId) : undefined));

  const cfg = summary?.config;
  const env = cfg?.env_tag ?? null;
  const text = cfg
    ? `${cfg.name} · ${info ? shortVersion(info) : 'not connected'}${cfg.ssh ? ` · via SSH ${cfg.ssh.host}` : ''}`
    : '';
  return (
    <footer className="statusbar">
      {env && (
        <span className={`status-env env-${env}`} data-testid="status-env" title={`Environment: ${env}`}>
          {ENV_LABEL[env]}
        </span>
      )}
      {cfg ? (
        <span className="status-conn">
          <span className={`dot${info ? ' on' : ''}`} aria-hidden="true" />
          <span title={info ? `${text}\n${info.version_string}` : text}>{text}</span>
          {database && (
            <span className="status-db" data-testid="status-db" title={`Database: ${database}`}>
              {database}
            </span>
          )}
          {cfg.read_only && (
            <span className="ro-badge" title="Read-only connection: statements that change data are blocked">
              <LockIcon />
              read-only
            </span>
          )}
        </span>
      ) : (
        <span className="status-conn">No connection selected</span>
      )}
      {queryTabId && <QueryStatus tabId={queryTabId} />}
      {queryTabId && (
        <span className="status-right">
          <CursorStatus tabId={queryTabId} />
          UTF-8
        </span>
      )}
      {objectTabId && <DataStatus tabId={objectTabId} />}
    </footer>
  );
}
