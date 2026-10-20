import { useConnections } from '../../store/connections';
import { ROW_LIMITS, dialectOfConnection, useQueryDocs, type QueryDoc, type RowLimit } from '../../store/queryTabs';
import { useTabs } from '../../store/tabs';
import { useTree } from '../../store/tree';

const SOON = 'Available once running is wired up';

/** Run / Stop / Explain, connection, database, row limit and continue-on-error of a query tab. */
export function QueryToolbar({ doc }: { doc: QueryDoc }) {
  const summaries = useConnections((s) => s.summaries);
  const nodes = useTree((s) => s.nodes);
  const update = useQueryDocs((s) => s.update);

  const prefix = doc.connectionId ? `conn:${doc.connectionId}/db:` : null;
  const databases = new Set<string>();
  if (prefix) {
    for (const id of nodes.keys()) {
      const raw = id.slice(prefix.length);
      if (!id.startsWith(prefix) || raw.includes('/')) continue;
      try {
        databases.add(decodeURIComponent(raw));
      } catch {
        databases.add(raw);
      }
    }
  }
  if (doc.database) databases.add(doc.database);

  const bind = (connectionId: string | null) => {
    update(doc.id, { connectionId, database: null, dialect: dialectOfConnection(connectionId) });
    useTabs.getState().bind(doc.id, connectionId);
  };
  // Running arrives with a later task; a tab without a connection must stay disabled then too.
  const canRun = false;

  return (
    <div className="query-toolbar" role="toolbar" aria-label="Query">
      <button type="button" className="qt-btn qt-run" data-testid="query-run" disabled={!canRun} title={SOON}>
        Run
      </button>
      <button type="button" className="qt-btn" data-testid="query-run-selection" disabled={!canRun} title={SOON}>
        Run selection
      </button>
      <button type="button" className="qt-btn" data-testid="query-stop" disabled title="Nothing is running">
        Stop
      </button>
      <button type="button" className="qt-btn" data-testid="query-explain" disabled={!canRun} title={SOON}>
        Explain
      </button>
      <span className="qt-sep" aria-hidden="true" />
      <select
        className="qt-select"
        data-testid="query-connection"
        aria-label="Connection"
        value={doc.connectionId ?? ''}
        onChange={(e) => bind(e.target.value || null)}
      >
        <option value="">Choose a connection</option>
        {doc.connectionId && !summaries.has(doc.connectionId) && (
          <option value={doc.connectionId} disabled>
            (deleted connection)
          </option>
        )}
        {[...summaries.values()].map((c) => (
          <option key={c.config.id} value={c.config.id}>
            {c.config.name}
          </option>
        ))}
      </select>
      <select
        className="qt-select"
        data-testid="query-database"
        aria-label="Database"
        value={doc.database ?? ''}
        disabled={!doc.connectionId}
        onChange={(e) => update(doc.id, { database: e.target.value || null })}
      >
        <option value="">Default database</option>
        {[...databases].map((d) => (
          <option key={d} value={d}>
            {d}
          </option>
        ))}
      </select>
      <label className="qt-field">
        <span>Limit</span>
        <select
          className="qt-select"
          data-testid="query-row-limit"
          aria-label="Row limit"
          value={doc.rowLimit}
          onChange={(e) => update(doc.id, { rowLimit: Number(e.target.value) as RowLimit })}
        >
          {ROW_LIMITS.map((n) => (
            <option key={n} value={n}>
              {n.toLocaleString('en-US')}
            </option>
          ))}
        </select>
      </label>
      <label className="qt-field">
        <input
          type="checkbox"
          data-testid="query-continue-on-error"
          checked={doc.continueOnError}
          onChange={(e) => update(doc.id, { continueOnError: e.target.checked })}
        />
        <span>Continue on error</span>
      </label>
    </div>
  );
}
