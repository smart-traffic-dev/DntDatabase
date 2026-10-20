import { useCallback, useEffect, useRef, useState, type MouseEvent } from 'react';
import { api, toAppError } from '../../api';
import type { HistoryEntry, HistoryStatus } from '../../bindings';
import { useConnections } from '../../store/connections';
import { useQueryDocs } from '../../store/queryTabs';
import { insertAtCursor } from '../../store/runQuery';

/** Search box debounce. */
export const SEARCH_DEBOUNCE_MS = 300;
/** Rows asked for per list (the backend clamps larger values). */
const LIST_LIMIT = 500;

// A cancelled statement may have taken effect, so it is never shown as "not applied".
const STATUS_TEXT: Record<HistoryStatus, string> = { ok: 'OK', error: 'Error', cancelled: 'Cancelled' };

const firstLine = (sql: string) => sql.split(/\r?\n/, 1)[0] ?? '';

/** `2026-10-20T08:15:30Z` as the local date and time. */
function timeText(iso: string): string {
  const d = new Date(iso);
  return Number.isNaN(d.getTime()) ? iso : d.toLocaleString('en-GB', { hour12: false });
}

/** The History panel of a query tab (spec 8.1). */
export function HistoryView({ tabId }: { tabId: string }) {
  const tabConnection = useQueryDocs((s) => s.docs.get(tabId)?.connectionId ?? null);
  const summaries = useConnections((s) => s.summaries);
  const [scope, setScope] = useState<'tab' | 'all'>('tab');
  const [searchText, setSearchText] = useState('');
  const [search, setSearch] = useState('');
  const [rows, setRows] = useState<HistoryEntry[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [selected, setSelected] = useState<Set<number>>(new Set());
  const [confirmClear, setConfirmClear] = useState(false);
  const anchor = useRef<number | null>(null);
  const seq = useRef(0);

  const connectionId = scope === 'tab' ? tabConnection : null;

  // Typing settles for SEARCH_DEBOUNCE_MS before it asks the backend.
  useEffect(() => {
    const timer = setTimeout(() => setSearch(searchText.trim()), SEARCH_DEBOUNCE_MS);
    return () => clearTimeout(timer);
  }, [searchText]);

  const load = useCallback(async () => {
    const mine = ++seq.current;
    try {
      const list = await api.historyList({
        connection_id: connectionId,
        search: search === '' ? null : search,
        limit: LIST_LIMIT,
      });
      if (mine !== seq.current) return;
      setRows(list);
      setError(null);
      setSelected((prev) => {
        const keep = new Set(list.filter((r) => prev.has(r.id)).map((r) => r.id));
        return keep.size === prev.size ? prev : keep;
      });
    } catch (e) {
      if (mine === seq.current) setError(toAppError(e).message);
    }
  }, [connectionId, search]);

  useEffect(() => {
    void load();
  }, [load]);

  const pick = (e: MouseEvent, row: HistoryEntry, index: number) => {
    if (e.shiftKey && anchor.current !== null) {
      const from = rows.findIndex((r) => r.id === anchor.current);
      const [a, b] = from < 0 ? [index, index] : [Math.min(from, index), Math.max(from, index)];
      setSelected(new Set(rows.slice(a, b + 1).map((r) => r.id)));
      return;
    }
    anchor.current = row.id;
    if (e.ctrlKey || e.metaKey) {
      setSelected((prev) => {
        const next = new Set(prev);
        if (!next.delete(row.id)) next.add(row.id);
        return next;
      });
    } else {
      setSelected(new Set([row.id]));
    }
  };

  const remove = async () => {
    try {
      await api.historyDelete([...selected]);
      setSelected(new Set());
      await load();
    } catch (e) {
      setError(toAppError(e).message);
    }
  };

  const clear = async () => {
    setConfirmClear(false);
    try {
      await api.historyClear();
      setSelected(new Set());
      await load();
    } catch (e) {
      setError(toAppError(e).message);
    }
  };

  return (
    <div className="hs-view" data-testid="history-view">
      <div className="hs-bar">
        <input
          type="search"
          className="hs-search"
          data-testid="history-search"
          aria-label="Search history"
          placeholder="Search history"
          value={searchText}
          onChange={(e) => setSearchText(e.target.value)}
        />
        <select
          className="qt-select"
          data-testid="history-connection"
          aria-label="Connection"
          value={scope === 'tab' && tabConnection ? tabConnection : ''}
          onChange={(e) => setScope(e.target.value === '' ? 'all' : 'tab')}
        >
          {tabConnection && <option value={tabConnection}>{summaries.get(tabConnection)?.config.name ?? tabConnection}</option>}
          <option value="">All connections</option>
        </select>
        <span className="spacer" />
        <button
          type="button"
          className="btn small"
          data-testid="history-delete"
          disabled={selected.size === 0}
          onClick={() => void remove()}
        >
          Delete
        </button>
        {confirmClear ? (
          <span className="hs-confirm" role="group" aria-label="Clear history">
            <span>Delete all history of every connection?</span>
            <button type="button" className="btn small danger" data-testid="history-clear-confirm" onClick={() => void clear()}>
              Clear
            </button>
            <button type="button" className="btn small" onClick={() => setConfirmClear(false)}>
              Cancel
            </button>
          </span>
        ) : (
          <button
            type="button"
            className="btn small"
            data-testid="history-clear"
            disabled={rows.length === 0}
            onClick={() => setConfirmClear(true)}
          >
            Clear
          </button>
        )}
      </div>
      {error && (
        <div className="qr-msg error" role="alert">
          {error}
        </div>
      )}
      <div className="hs-list" role="listbox" aria-multiselectable="true" aria-label="History">
        {rows.length === 0 && !error && <div className="qr-empty">No history.</div>}
        {rows.map((row, i) => (
          <div
            key={row.id}
            role="option"
            aria-selected={selected.has(row.id)}
            className={`hs-row ${row.status}${selected.has(row.id) ? ' selected' : ''}`}
            data-testid="history-row"
            title={row.sql_text}
            onClick={(e) => pick(e, row, i)}
            onDoubleClick={() => insertAtCursor(tabId, row.sql_text)}
          >
            <span className="hs-time">{timeText(row.started_at)}</span>
            <span className={`hs-status ${row.status}`}>{STATUS_TEXT[row.status]}</span>
            <span className="hs-elapsed">{row.elapsed_ms === null ? '' : `${row.elapsed_ms} ms`}</span>
            <span className="hs-sql">{firstLine(row.sql_text)}</span>
            {row.redacted && <span className="chip hs-redacted">redacted</span>}
          </div>
        ))}
      </div>
    </div>
  );
}
