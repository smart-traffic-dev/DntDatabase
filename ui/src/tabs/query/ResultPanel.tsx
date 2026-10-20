import { memo, useEffect, useRef, useState } from 'react';
import { queryExportRequest, requestExport, shownResult } from '../../dialogs/resultExport';
import { plural } from '../../lib/plural';
import { dialectOfConnection, useQueryDocs } from '../../store/queryTabs';
import { canRun, emptyRun, useQueryRun, type ResultSet, type TabRun } from '../../store/queryRun';
import { MEMORY_WARNING_ROWS } from './format';
import { ExplainView } from './ExplainView';
import { HistoryView } from './HistoryView';
import { MessagesView } from './MessagesView';
import { ResultGrid } from './ResultGrid';
import { PLAN_GONE, rerunStatement, usePlanAlive } from './rerun';

/** Row limit "Fetch all" runs with. */
const FETCH_ALL_ROWS = 1_000_000;

function Summary({ result }: { result: ResultSet }) {
  const text = result.affected !== null ? `${result.affected} rows affected` : `${result.rowCount} rows`;
  return (
    <span className="qr-summary" data-testid="result-summary">
      {text}
      {result.elapsedMs !== null && ` · ${result.elapsedMs} ms`}
    </span>
  );
}

/** Statements that failed (a plain user cancel is not one). */
const errorCount = (run: TabRun): number =>
  run.messages.reduce((n, m) => (m.index !== null && m.error !== null && m.tone !== 'info' ? n + 1 : n), 0);

/** "Cancelled by user after 3.1 s" when the user stopped the run; null otherwise. */
function cancelledText(run: TabRun): string | null {
  if (run.stop?.kind !== 'cancelled') return null;
  const ms = run.messages.findLast((m) => m.index === null)?.elapsedMs ?? null;
  return ms === null ? 'Cancelled by user' : `Cancelled by user after ${(ms / 1000).toFixed(1)} s`;
}

/** The bottom half of a query tab: result tabs and Messages. Memo: typing re-renders its parent. */
export const ResultPanel = memo(ResultPanelView);

function ResultPanelView({ tabId }: { tabId: string }) {
  const run = useQueryRun((s) => s.runs.get(tabId) ?? emptyRun);
  const setPanel = useQueryRun((s) => s.setPanel);
  const planAlive = usePlanAlive(tabId);
  // The engine only quotes copied INSERT statements; a primitive, so typing does not re-render this.
  const connectionId = useQueryDocs((s) => s.docs.get(tabId)?.connectionId ?? null);
  const engine = dialectOfConnection(connectionId);
  const stripRef = useRef<HTMLDivElement>(null);
  // Which result tab is open; reset by `key` when a new run starts.
  const [picked, setPicked] = useState<{ runId: string | null; at: number }>({ runId: run.runId, at: 0 });
  const at = Math.min(picked.runId === run.runId ? picked.at : 0, Math.max(0, run.results.length - 1));
  const result = run.results[at];
  const showResult = run.panel === 'result' && result !== undefined;
  const showExplain = run.panel === 'explain';
  const showHistory = run.panel === 'history';
  const hasExplain = run.explain !== null || run.explainError !== null;
  const readStatement = result ? run.plan?.stmts.find((s) => s.index === result.index)?.kind === 'read' : false;
  const errors = errorCount(run);
  const cancelled = cancelledText(run);

  // A run that ends on an error after a result moves to Messages: the squiggle alone is too quiet (F16).
  const wasActive = useRef(run.phase.kind !== 'idle');
  useEffect(() => {
    const active = run.phase.kind !== 'idle';
    const ended = wasActive.current && !active;
    wasActive.current = active;
    if (ended && run.stop?.kind === 'error' && run.panel === 'result') setPanel(tabId, 'messages');
  }, [run.phase.kind, run.stop, run.panel, setPanel, tabId]);

  // `openExportDialog` (menu hook) exports the result shown here.
  useEffect(() => {
    shownResult.set(tabId, at);
  }, [tabId, at]);

  // With dozens of result tabs the strip scrolls inside itself; keep the open one in view.
  useEffect(() => {
    stripRef.current?.querySelector('[aria-selected="true"]')?.scrollIntoView?.({ block: 'nearest', inline: 'nearest' });
  }, [at, showResult]);

  return (
    <div className="qr-panel" data-testid="result-panel">
      <div className="qr-tabs" role="tablist">
        <div className="qr-tabs-scroll" role="presentation" data-testid="result-tabs" ref={stripRef}>
          {run.results.map((r, i) => (
            <button
              key={r.id}
              type="button"
              role="tab"
              aria-selected={showResult && i === at}
              className={`qr-tab${showResult && i === at ? ' active' : ''}`}
              data-testid={`result-tab-${i + 1}`}
              onClick={() => {
                setPicked({ runId: run.runId, at: i });
                setPanel(tabId, 'result');
              }}
            >
              Result {i + 1}
            </button>
          ))}
        </div>
        <button
          type="button"
          role="tab"
          aria-selected={!showResult && !showExplain && !showHistory}
          aria-label={errors > 0 ? `Messages (${plural(errors, 'error')})` : undefined}
          className={`qr-tab${!showResult && !showExplain && !showHistory ? ' active' : ''}`}
          data-testid="panel-messages"
          onClick={() => setPanel(tabId, 'messages')}
        >
          Messages
          {errors > 0 && (
            <span className="qr-badge" data-testid="messages-badge" aria-hidden="true">
              {errors}
            </span>
          )}
        </button>
        {(hasExplain || showExplain) && (
          <button
            type="button"
            role="tab"
            aria-selected={showExplain}
            className={`qr-tab${showExplain ? ' active' : ''}`}
            data-testid="panel-explain"
            onClick={() => setPanel(tabId, 'explain')}
          >
            Explain
          </button>
        )}
        <button
          type="button"
          role="tab"
          aria-selected={showHistory}
          className={`qr-tab${showHistory ? ' active' : ''}`}
          data-testid="panel-history"
          onClick={() => setPanel(tabId, 'history')}
        >
          History
        </button>
        {showResult && <Summary result={result} />}
        {showResult && result.columns.length > 0 && (
          <button
            type="button"
            className="btn small"
            style={{ marginLeft: 8, flex: 'none' }}
            data-testid="result-export"
            title="Export this result to a file"
            onClick={() => requestExport(queryExportRequest(tabId, at))}
          >
            Export…
          </button>
        )}
      </div>
      {showResult ? (
        <>
          {cancelled && (
            <div className="qr-banner" role="status" data-testid="result-cancelled">
              <span>{cancelled}; the results shown may be incomplete.</span>
            </div>
          )}
          {result.truncated && (
            <div className="qr-banner" role="status">
              <span>Showing first {result.rowCount} rows</span>
              {readStatement && result.rowCount < FETCH_ALL_ROWS && (
                <button
                  type="button"
                  className="btn small"
                  disabled={!canRun(run) || !planAlive}
                  title={planAlive ? undefined : PLAN_GONE}
                  onClick={() => void rerunStatement(tabId, result.index, { maxRows: FETCH_ALL_ROWS })}
                >
                  Fetch all
                </button>
              )}
            </div>
          )}
          {result.rowCount > MEMORY_WARNING_ROWS && (
            <div className="qr-banner warning" role="status">
              Large result: {result.rowCount} rows are kept in memory.
            </div>
          )}
          <ResultGrid key={result.id} result={result} engine={engine} />
        </>
      ) : showExplain ? (
        <ExplainView tabId={tabId} />
      ) : showHistory ? (
        <HistoryView tabId={tabId} />
      ) : (
        <MessagesView tabId={tabId} run={run} />
      )}
    </div>
  );
}
