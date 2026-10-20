import { useCallback, useEffect, useRef, useState, type KeyboardEvent } from 'react';
import { api, toAppError } from '../../api';
import type { HistoryEntry, HistoryStatus } from '../../bindings';
import { useConnections } from '../../store/connections';
import { useQueryDocs } from '../../store/queryTabs';
import { insertAtCursor } from '../../store/runQuery';
import './toolbar.css';

/** Search box debounce. */
export const SEARCH_DEBOUNCE_MS = 300;
/** Rows asked for per list (the backend clamps larger values). */
const LIST_LIMIT = 500;

/** Error kinds after which the server may or may not have applied the statement. */
const UNKNOWN_OUTCOME_KINDS = new Set(['timeout', 'connection_lost']);

/**
 * The status cell of a row. A statement stopped by a timeout, a lost connection or a cancel may
 * have taken effect, so it never reads as a plain error ("not applied").
 */
export function statusOf(row: Pick<HistoryEntry, 'status' | 'error_kind' | 'error_code'>): {
  text: string;
  tone: HistoryStatus | 'unknown';
  hint: string | undefined;
} {
  const code = row.error_code ? ` (${row.error_code})` : '';
  if (row.status === 'ok') return { text: 'OK', tone: 'ok', hint: undefined };
  if (row.error_kind !== null && UNKNOWN_OUTCOME_KINDS.has(row.error_kind)) {
    const why = row.error_kind === 'timeout' ? 'timed out' : 'lost its connection';
    return {
      text: `Unknown outcome${code}`,
      tone: 'unknown',
      hint: `The statement ${why} before the server confirmed it: it may have been applied. Check the data before running it again.`,
    };
  }
  if (row.status === 'cancelled' || row.error_kind === 'cancelled') {
    return {
      text: `Cancelled — may have been applied${code}`,
      tone: 'cancelled',
      hint: 'The statement was cancelled while it ran: the server may have applied it before the cancel arrived.',
    };
  }
  return {
    text: `Error${code}`,
    tone: 'error',
    hint: row.error_kind ? `Failed (${row.error_kind}${row.error_code ? `, code ${row.error_code}` : ''}).` : undefined,
  };
}

type Modifiers = Pick<KeyboardEvent, 'shiftKey' | 'ctrlKey' | 'metaKey'>;

const firstLine =(sql: string) => sql.split(/\r?\n/, 1)[0] ?? '';

/**
 * An Explain run is stored as its wrapper (`EXPLAIN [ANALYZE] [FORMAT=JSON | (ANALYZE, FORMAT JSON)] <stmt>`,
 * or MariaDB's `ANALYZE [FORMAT=JSON] <stmt>`): the statement inside, and whether it was an Analyze.
 */
function explainOf(row: Pick<HistoryEntry, 'sql_text'>): { inner: string; analyze: boolean } | null {
  const m = /^\s*(EXPLAIN|ANALYZE)\s+(?:FORMAT\s*=\s*\w+\s+|\(([^)]*)\)\s+|(ANALYZE\s+)(?:FORMAT\s*=\s*\w+\s+)?)?/i.exec(row.sql_text);
  if (!m) return null;
  const analyze = m[1]!.toUpperCase() === 'ANALYZE' || !!m[3] || /\bANALYZE\b/i.test(m[2] ?? '');
  return { inner: row.sql_text.slice(m[0].length), analyze };
}

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
  /** The row that takes Tab focus (roving tabIndex). */
  const [active, setActive] = useState(0);
  const anchor = useRef<number | null>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const seq = useRef(0);
  const focusable = Math.min(active, Math.max(rows.length - 1, 0));

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

  const pick = (e: Modifiers, row: HistoryEntry, index: number) => {
    setActive(index);
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

  /** Moves the focused row (roving tabIndex), selecting it unless Ctrl is held; Shift extends. */
  const moveTo = (e: KeyboardEvent, index: number) => {
    const to = Math.min(Math.max(index, 0), rows.length - 1);
    if (to < 0) return;
    e.preventDefault();
    if (e.ctrlKey || e.metaKey) setActive(to);
    else pick(e, rows[to]!, to);
    listRef.current?.querySelectorAll<HTMLElement>('[role="option"]')[to]?.focus();
  };

  const onRowKey = (e: KeyboardEvent, row: HistoryEntry, index: number) => {
    switch (e.key) {
      case 'ArrowDown':
        return moveTo(e, index + 1);
      case 'ArrowUp':
        return moveTo(e, index - 1);
      case 'Home':
        return moveTo(e, 0);
      case 'End':
        return moveTo(e, rows.length - 1);
      case 'Enter':
        e.preventDefault();
        insertAtCursor(tabId, row.sql_text);
        return;
      case ' ':
        e.preventDefault();
        pick({ shiftKey: e.shiftKey, ctrlKey: true, metaKey: false }, row, index);
        return;
      case 'Delete':
        e.preventDefault();
        // The selection, or the focused row when nothing is selected.
        void remove(selected.size > 0 ? [...selected] : [row.id]);
        return;
    }
  };

  const remove = async (ids: number[] = [...selected]) => {
    try {
      await api.historyDelete(ids);
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
      <div
        ref={listRef}
        className="hs-list"
        role="listbox"
        aria-multiselectable="true"
        aria-label="History"
        aria-describedby={`hs-keys-${tabId}`}
      >
        <span id={`hs-keys-${tabId}`} hidden>
          Arrow keys move, Enter inserts the statement at the cursor, Space selects, Delete deletes.
        </span>
        {rows.length === 0 && !error && <div className="qr-empty">No history.</div>}
        {rows.map((row, i) => {
          const status = statusOf(row);
          const explain = explainOf(row);
          return (
            <div
              key={row.id}
              role="option"
              aria-selected={selected.has(row.id)}
              tabIndex={i === focusable ? 0 : -1}
              className={`hs-row ${status.tone}${selected.has(row.id) ? ' selected' : ''}`}
              data-testid="history-row"
              title={row.sql_text}
              onClick={(e) => pick(e, row, i)}
              onDoubleClick={() => insertAtCursor(tabId, row.sql_text)}
              onKeyDown={(e) => onRowKey(e, row, i)}
            >
              <span className="hs-time">{timeText(row.started_at)}</span>
              <span className={`hs-status ${status.tone}`} data-testid="history-status" title={status.hint}>
                {status.text}
              </span>
              <span className="hs-elapsed">{row.elapsed_ms === null ? '' : `${row.elapsed_ms} ms`}</span>
              {scope === 'all' && (
                <span className="hs-origin" data-testid="history-origin">
                  {summaries.get(row.connection_id)?.config.name ?? row.connection_id}
                  {row.database ? ` · ${row.database}` : ''}
                </span>
              )}
              {explain && <span className="chip hs-explain" data-testid="history-explain">{explain.analyze ? 'Analyze' : 'Explain'}</span>}
              <span className="hs-sql">{firstLine(explain ? explain.inner : row.sql_text)}</span>
              {row.redacted && <span className="chip hs-redacted">redacted</span>}
            </div>
          );
        })}
      </div>
    </div>
  );
}
