import { useEffect, useState } from 'react';
import { useConnections } from '../store/connections';
import { canRun, emptyRun, useQueryRun } from '../store/queryRun';
import { txAction } from '../store/runQuery';
import { useTabs } from '../store/tabs';
import { useTree } from '../store/tree';
import { connectionIdOf } from '../store/treeModel';
import { LockIcon } from './icons';
import { formatElapsed, shortVersion } from './statusFormat';

/** Seconds since `since`, ticking once a second while mounted. */
function useElapsedSeconds(since: number | null): number | null {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (since === null) return;
    setNow(Date.now());
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, [since]);
  return since === null ? null : Math.max(0, Math.floor((now - since) / 1000));
}

/** Last run, transaction state and run/stop progress of a query tab (spec §6.2). */
function QueryStatus({ tabId }: { tabId: string }) {
  const run = useQueryRun((s) => s.runs.get(tabId) ?? emptyRun);
  const phase = run.phase;
  const since = phase.kind === 'idle' ? null : phase.kind === 'running' ? phase.startedAt : phase.since;
  const seconds = useElapsedSeconds(since);
  const idle = canRun(run);
  const cancelled = idle && run.stop?.kind === 'cancelled';
  return (
    <>
      {phase.kind === 'running' && seconds !== null && (
        <span className="status-run running" data-testid="status-running">
          <span className="spinner inline" aria-hidden="true" />
          {/* Static text only: the live region announces statement changes, not every tick. */}
          <span role="status" data-testid="status-running-live">
            Running… {phase.index + 1}/{phase.total}
          </span>
          <span aria-hidden="true"> · {formatElapsed(seconds * 1000)}</span>
        </span>
      )}
      {idle && run.lastRun && (
        <span className={`status-last${cancelled ? ' cancelled' : ''}`} data-testid="status-last">
          {cancelled
            ? `Cancelled by user after ${formatElapsed(run.lastRun.elapsedMs)}`
            : `${run.lastRun.rows.toLocaleString('en-US')} ${run.lastRun.rows === 1 ? 'row' : 'rows'} · ${run.lastRun.elapsedMs.toLocaleString('en-US')} ms`}
        </span>
      )}
      {idle && !run.lastRun && cancelled && (
        <span className="status-last cancelled" data-testid="status-last">
          Cancelled by user
        </span>
      )}
      <span className={`status-tx tx-${run.txState}`} data-testid="status-tx">
        {run.txState === 'idle' && 'Auto-commit'}
        {run.txState === 'active' && 'In transaction'}
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
      {(phase.kind === 'stopping' || phase.kind === 'abandoning') && seconds !== null && (
        <span className="status-run" data-testid="status-run">
          {phase.kind === 'abandoning'
            ? `Server not responding — replacing connection… ${seconds} s`
            : `Stopping… ${seconds} s`}
        </span>
      )}
    </>
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
  const tabConnection = useTabs((s) => s.tabs.find((t) => t.id === s.activeId)?.connectionId);
  // A query tab without a connection falls back to the tree selection.
  const connectionId = tabConnection ?? (selectedId ? connectionIdOf(selectedId) : undefined);
  const summary = useConnections((s) => (connectionId ? s.summaries.get(connectionId) : undefined));
  const info = useConnections((s) => (connectionId ? s.info.get(connectionId) : undefined));

  const cfg = summary?.config;
  const production = cfg?.env_tag === 'production';
  const text = cfg
    ? `${cfg.name} · ${info ? shortVersion(info) : 'not connected'}${cfg.ssh ? ` · via SSH ${cfg.ssh.host}` : ''}`
    : '';
  return (
    <footer className={`statusbar${production ? ' env-production' : ''}`}>
      {cfg ? (
        <span className="status-conn">
          <span className={`dot${info ? ' on' : ''}`} aria-hidden="true" />
          <span title={info ? `${text}\n${info.version_string}` : text}>{text}</span>
          {production && <span className="env-badge">PRODUCTION</span>}
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
      <span className="status-right">UTF-8</span>
    </footer>
  );
}
