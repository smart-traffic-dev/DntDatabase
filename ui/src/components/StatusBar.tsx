import { useEffect, useState } from 'react';
import { useConnections } from '../store/connections';
import { canRun, emptyRun, useQueryRun } from '../store/queryRun';
import { txAction } from '../store/runQuery';
import { useTabs } from '../store/tabs';
import { useTree } from '../store/tree';
import { connectionIdOf } from '../store/treeModel';

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

/** Last run, transaction state and stop progress of a query tab (spec §6.2). */
function QueryStatus({ tabId }: { tabId: string }) {
  const run = useQueryRun((s) => s.runs.get(tabId) ?? emptyRun);
  const since = run.phase.kind === 'stopping' || run.phase.kind === 'abandoning' ? run.phase.since : null;
  const seconds = useElapsedSeconds(since);
  const idle = canRun(run);
  return (
    <>
      {run.lastRun && (
        <span className="status-last">
          {run.lastRun.rows} rows · {run.lastRun.elapsedMs} ms
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
      {seconds !== null && (
        <span className="status-run" data-testid="status-run">
          {run.phase.kind === 'abandoning'
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
  return (
    <footer className={`statusbar${production ? ' env-production' : ''}`}>
      {cfg ? (
        <span className="status-conn">
          <span className={`dot${info ? ' on' : ''}`} aria-hidden="true" />
          <span>
            {cfg.name}
            {info ? ` · ${info.version_string}` : ' · not connected'}
            {cfg.ssh ? ` · via SSH ${cfg.ssh.host}` : ''}
          </span>
          {production && <span className="env-badge">PRODUCTION</span>}
        </span>
      ) : (
        <span className="status-conn">No connection selected</span>
      )}
      {queryTabId && <QueryStatus tabId={queryTabId} />}
      <span className="status-right">UTF-8</span>
    </footer>
  );
}
