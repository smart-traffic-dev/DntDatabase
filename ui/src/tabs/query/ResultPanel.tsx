import { useState } from 'react';
import { canRun, emptyRun, useQueryRun, type ResultSet } from '../../store/queryRun';
import { MEMORY_WARNING_ROWS } from './format';
import { MessagesView } from './MessagesView';
import { ResultGrid } from './ResultGrid';
import { rerunStatement } from './rerun';

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

/** The bottom half of a query tab: result tabs and Messages. */
export function ResultPanel({ tabId }: { tabId: string }) {
  const run = useQueryRun((s) => s.runs.get(tabId) ?? emptyRun);
  const setPanel = useQueryRun((s) => s.setPanel);
  // Which result tab is open; reset by `key` when a new run starts.
  const [picked, setPicked] = useState<{ runId: string | null; at: number }>({ runId: run.runId, at: 0 });
  const at = Math.min(picked.runId === run.runId ? picked.at : 0, Math.max(0, run.results.length - 1));
  const result = run.results[at];
  const showResult = run.panel !== 'messages' && result !== undefined;
  const readStatement = result ? run.plan?.stmts.find((s) => s.index === result.index)?.kind === 'read' : false;

  return (
    <div className="qr-panel" data-testid="result-panel">
      <div className="qr-tabs" role="tablist">
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
        <button
          type="button"
          role="tab"
          aria-selected={!showResult}
          className={`qr-tab${!showResult ? ' active' : ''}`}
          data-testid="panel-messages"
          onClick={() => setPanel(tabId, 'messages')}
        >
          Messages
        </button>
        {showResult && <Summary result={result} />}
      </div>
      {showResult ? (
        <>
          {result.truncated && (
            <div className="qr-banner" role="status">
              <span>Showing first {result.rowCount} rows</span>
              {readStatement && result.rowCount < FETCH_ALL_ROWS && (
                <button
                  type="button"
                  className="btn small"
                  disabled={!canRun(run)}
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
          <ResultGrid key={result.id} result={result} />
        </>
      ) : (
        <MessagesView tabId={tabId} run={run} />
      )}
    </div>
  );
}
