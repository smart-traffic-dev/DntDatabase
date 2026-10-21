import { useCallback, useEffect, useId, useMemo, useRef, useState, type KeyboardEvent, type MouseEvent } from 'react';
import { useShallow } from 'zustand/react/shallow';
import { api, toAppError } from '../api';
import type { HistoryEntry } from '../bindings';
import { useConnections } from '../store/connections';
import { useQueryRun } from '../store/queryRun';
import { useQueryDocs } from '../store/queryTabs';
import { insertAtCursor, runQuery } from '../store/runQuery';
import { useTabs } from '../store/tabs';
import { editorOf } from '../tabs/query/activeEditor';
import { ActionMenu, type MenuEntry } from './ActionMenu';
import {
  NOT_RE_RUNNABLE,
  canReRun,
  detailOf,
  explainOf,
  filterByStatus,
  firstLine,
  groupByDay,
  statusOf,
  timeText,
  type StatusFilter,
} from './historyModel';
import { ChevronIcon, TrashIcon } from './icons';

/** Search box debounce. */
export const SEARCH_DEBOUNCE_MS = 300;
/** Rows asked for per list (the backend clamps larger values). */
const LIST_LIMIT = 500;

const SEGMENTS: { id: StatusFilter; label: string }[] = [
  { id: 'all', label: 'All' },
  { id: 'ok', label: 'OK' },
  { id: 'errors', label: 'Errors' },
];

type Modifiers = Pick<KeyboardEvent, 'shiftKey' | 'ctrlKey' | 'metaKey'>;

const copy = (text: string) => void navigator.clipboard?.writeText(text).catch(() => undefined);

/**
 * The History panel of the right sidebar (spec §3): every statement the app ran, newest first,
 * filtered by search, connection (the focused tab's by default) and outcome. A click unfolds the
 * full statement with Insert / Open in new tab / Run again / Copy.
 */
export function HistoryPanel({ onCollapse }: { onCollapse?: () => void }) {
  const summaries = useConnections((s) => s.summaries);
  const focus = useTabs(
    useShallow((s) => {
      const tab = s.tabs.find((t) => t.id === s.activeId);
      return { id: tab?.id ?? null, kind: tab?.kind ?? null, connectionId: tab?.kind === 'object' ? tab.connectionId : null };
    }),
  );
  const docConnection = useQueryDocs((s) => (focus.kind === 'query' && focus.id ? (s.docs.get(focus.id)?.connectionId ?? null) : null));
  const focusConnection = focus.kind === 'query' ? docConnection : focus.connectionId;
  /** The tab Insert writes to: the focused editor's, when the focused tab is a query tab. */
  const insertTab = focus.kind === 'query' ? focus.id : null;
  // A run that ended adds a history row: the list reloads when the number of finished runs changes.
  const finishedRuns = useQueryRun((s) => {
    let n = 0;
    for (const r of s.runs.values()) if (r.runId !== null && r.phase.kind === 'idle') n++;
    return n;
  });

  const [scope, setScope] = useState<'focus' | 'all'>('focus');
  const [status, setStatus] = useState<StatusFilter>('all');
  const [searchText, setSearchText] = useState('');
  const [search, setSearch] = useState('');
  const [rows, setRows] = useState<HistoryEntry[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [selected, setSelected] = useState<Set<number>>(new Set());
  const [expanded, setExpanded] = useState<number | null>(null);
  const [confirmClear, setConfirmClear] = useState(false);
  const [menu, setMenu] = useState<{ x: number; y: number; row: HistoryEntry } | null>(null);
  /** The row that takes Tab focus (roving tabIndex). */
  const [active, setActive] = useState(0);
  const anchor = useRef<number | null>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const seq = useRef(0);
  const keyMenuAt = useRef(0);
  const keysId = useId();

  const connectionId = scope === 'focus' ? focusConnection : null;
  const allScope = connectionId === null;

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
        origin: null,
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
  }, [load, finishedRuns]);

  const shown = useMemo(() => filterByStatus(rows, status), [rows, status]);
  const groups = useMemo(() => groupByDay(shown), [shown]);
  /** The rows in the order they are drawn (groups are contiguous slices of the list). */
  const flat = useMemo(() => groups.flatMap((g) => g.rows), [groups]);
  const focusable = Math.min(active, Math.max(flat.length - 1, 0));
  const indexOf = useMemo(() => new Map(flat.map((r, i) => [r.id, i])), [flat]);
  const now = new Date();

  const pick = (e: Modifiers, row: HistoryEntry, index: number) => {
    setActive(index);
    if (e.shiftKey && anchor.current !== null) {
      const from = flat.findIndex((r) => r.id === anchor.current);
      const [a, b] = from < 0 ? [index, index] : [Math.min(from, index), Math.max(from, index)];
      setSelected(new Set(flat.slice(a, b + 1).map((r) => r.id)));
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
    const to = Math.min(Math.max(index, 0), flat.length - 1);
    if (to < 0) return;
    e.preventDefault();
    if (e.ctrlKey || e.metaKey) setActive(to);
    else pick(e, flat[to]!, to);
    listRef.current?.querySelectorAll<HTMLElement>('[role="option"]')[to]?.focus();
  };

  const say = (text: string | null) => setNotice(text);

  /** Inserts a row's statement at the caret of the focused editor. */
  const insert = (row: HistoryEntry) => {
    if (!canReRun(row)) return say(NOT_RE_RUNNABLE);
    if (!insertTab) return say('Open a query tab to insert a statement.');
    say(null);
    insertAtCursor(insertTab, row.sql_text);
    editorOf(insertTab)?.focus();
  };

  /** A new query tab on the entry's connection and database, holding its statement. */
  const openInNewTab = (row: HistoryEntry): string | null => {
    if (!canReRun(row)) {
      say(NOT_RE_RUNNABLE);
      return null;
    }
    say(null);
    const id = useQueryDocs.getState().newQuery(summaries.has(row.connection_id) ? row.connection_id : null, row.database);
    useQueryDocs.getState().update(id, { sql: row.sql_text });
    return id;
  };

  /** Run again: a new tab, run through the normal path so the plan, the guards and the Production prompt apply. */
  const runAgain = (row: HistoryEntry) => {
    if (!canReRun(row)) return say(NOT_RE_RUNNABLE);
    if (!summaries.has(row.connection_id)) return say('The connection of this statement no longer exists.');
    const id = openInNewTab(row);
    if (id) void runQuery(id, 'All');
  };

  const remove = async (ids: number[]) => {
    try {
      await api.historyDelete(ids);
      setSelected(new Set());
      if (expanded !== null && ids.includes(expanded)) setExpanded(null);
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
      setExpanded(null);
      await load();
    } catch (e) {
      setError(toAppError(e).message);
    }
  };

  /** The selection when `row` is part of it, else just `row`. */
  const targetsOf = (row: HistoryEntry) => (selected.has(row.id) && selected.size > 1 ? [...selected] : [row.id]);

  const menuItems = (row: HistoryEntry): MenuEntry[] => {
    const rerunnable = canReRun(row);
    return [
      {
        label: 'Insert',
        hint: 'Enter',
        disabled: !rerunnable || !insertTab,
        title: !rerunnable ? NOT_RE_RUNNABLE : !insertTab ? 'Open a query tab to insert a statement.' : undefined,
        run: () => insert(row),
      },
      { label: 'Open in new tab', disabled: !rerunnable, title: rerunnable ? undefined : NOT_RE_RUNNABLE, run: () => void openInNewTab(row) },
      { label: 'Run again', disabled: !rerunnable, title: rerunnable ? undefined : NOT_RE_RUNNABLE, run: () => runAgain(row) },
      { label: 'Copy', run: () => copy(row.sql_text) },
      { separator: true },
      { label: 'Delete', hint: 'Del', danger: true, run: () => void remove(targetsOf(row)) },
    ];
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
        return moveTo(e, flat.length - 1);
      case 'ArrowRight':
        e.preventDefault();
        setExpanded(row.id);
        return;
      case 'ArrowLeft':
        e.preventDefault();
        setExpanded(null);
        return;
      case 'Enter':
        e.preventDefault();
        insert(row);
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
      case 'ContextMenu':
      case 'F10': {
        if (e.key === 'F10' && !e.shiftKey) return;
        e.preventDefault();
        keyMenuAt.current = Date.now();
        const box = (e.currentTarget as HTMLElement).getBoundingClientRect();
        setMenu({ x: box.left + 24, y: box.bottom, row });
        return;
      }
    }
  };

  // WebView2 fires `contextmenu` (at 0,0) after the menu key's keydown; keep the keyboard-placed menu.
  const onContextMenu = (e: MouseEvent, row: HistoryEntry, index: number) => {
    e.preventDefault();
    if (Date.now() - keyMenuAt.current < 300) return;
    if (!selected.has(row.id)) pick(e, row, index);
    setMenu({ x: e.clientX, y: e.clientY, row });
  };

  const onRowClick = (e: MouseEvent, row: HistoryEntry, index: number) => {
    pick(e, row, index);
    // A double click unfolds on its first click and inserts on the dblclick; its second click leaves it open.
    if (e.shiftKey || e.ctrlKey || e.metaKey || e.detail > 1) return;
    setExpanded((cur) => (cur === row.id ? null : row.id));
  };

  const focusName = focusConnection ? (summaries.get(focusConnection)?.config.name ?? focusConnection) : null;

  return (
    <section className="hs-view" aria-label="History" data-testid="history-view">
      <div className="rs-head">
        <strong className="rs-title">HISTORY</strong>
        <span className="spacer" />
        <button
          type="button"
          className="btn small"
          data-testid="history-delete"
          disabled={selected.size === 0}
          onClick={() => void remove([...selected])}
        >
          Delete
        </button>
        <button
          type="button"
          className="icon-btn"
          data-testid="history-clear"
          aria-label="Clear history"
          title="Clear history"
          disabled={flat.length === 0 && rows.length === 0}
          onClick={() => setConfirmClear(true)}
        >
          <TrashIcon />
        </button>
        {onCollapse && (
          <button
            type="button"
            className="icon-btn"
            data-testid="right-sidebar-collapse"
            aria-label="Collapse right sidebar"
            title="Collapse right sidebar (Ctrl+Alt+B)"
            onClick={onCollapse}
          >
            <ChevronIcon />
          </button>
        )}
      </div>
      {confirmClear && (
        <div className="hs-confirm" role="group" aria-label="Confirm clearing history">
          <span>Delete all history of every connection?</span>
          <button type="button" className="btn small danger" data-testid="history-clear-confirm" onClick={() => void clear()}>
            Clear
          </button>
          <button type="button" className="btn small" onClick={() => setConfirmClear(false)}>
            Cancel
          </button>
        </div>
      )}
      <div className="hs-filters">
        <input
          type="search"
          className="hs-search"
          data-testid="history-search"
          aria-label="Search history"
          placeholder="Search SQL…"
          value={searchText}
          onChange={(e) => setSearchText(e.target.value)}
        />
        <div className="hs-filter-row">
          <select
            className="hs-select"
            data-testid="history-connection"
            aria-label="Connection"
            value={focusConnection && scope === 'focus' ? focusConnection : ''}
            onChange={(e) => setScope(e.target.value === '' ? 'all' : 'focus')}
          >
            {focusConnection && <option value={focusConnection}>{focusName}</option>}
            <option value="">All connections</option>
          </select>
          <div className="hs-segment" role="radiogroup" aria-label="Status">
            {SEGMENTS.map((s) => (
              <button
                key={s.id}
                type="button"
                role="radio"
                aria-checked={status === s.id}
                className={`hs-seg${status === s.id ? ' on' : ''}`}
                onClick={() => setStatus(s.id)}
              >
                {s.label}
              </button>
            ))}
          </div>
        </div>
      </div>
      {error && (
        <div className="qr-msg error" role="alert">
          {error}
        </div>
      )}
      {notice && (
        <div className="qr-msg" role="status">
          {notice}
        </div>
      )}
      <div ref={listRef} className="hs-list" role="listbox" aria-multiselectable="true" aria-label="History entries" aria-describedby={keysId}>
        <span id={keysId} hidden>
          Arrow keys move, Right and Left unfold and fold, Enter inserts the statement at the cursor, Space selects, Delete deletes.
        </span>
        {flat.length === 0 && !error && <div className="qr-empty">No history.</div>}
        {groups.map((group) => (
          <div key={group.label} role="group" aria-label={group.label}>
            <div className="hs-group" aria-hidden="true">
              {group.label}
            </div>
            {group.rows.map((row) => {
              const i = indexOf.get(row.id) ?? 0;
              const st = statusOf(row);
              const explain = explainOf(row);
              const open = expanded === row.id;
              return (
                <div key={row.id} className="hs-item">
                  <div
                    role="option"
                    aria-selected={selected.has(row.id)}
                    aria-expanded={open}
                    tabIndex={i === focusable ? 0 : -1}
                    className={`hs-row ${st.tone}${selected.has(row.id) ? ' selected' : ''}`}
                    data-testid="history-row"
                    onClick={(e) => onRowClick(e, row, i)}
                    onDoubleClick={() => insert(row)}
                    onContextMenu={(e) => onContextMenu(e, row, i)}
                    onKeyDown={(e) => onRowKey(e, row, i)}
                  >
                    <div className="hs-top">
                      <span className={`hs-dot ${st.tone}`} role="img" aria-label={st.text} title={st.hint ?? st.text} data-testid="history-status" />
                      <span className="hs-time">{timeText(row.started_at, now)}</span>
                      <span className="hs-elapsed">{row.elapsed_ms === null ? '' : `${row.elapsed_ms} ms`}</span>
                      <span className={`hs-result ${st.tone}`}>{detailOf(row)}</span>
                      <span className="spacer" />
                      {allScope ? (
                        <span className="hs-origin" data-testid="history-origin">
                          {summaries.get(row.connection_id)?.config.name ?? row.connection_id}
                          {row.database ? ` · ${row.database}` : ''}
                        </span>
                      ) : (
                        row.database && <span className="hs-origin">{row.database}</span>
                      )}
                      {row.origin === 'data_edit' && (
                        <span className="chip hs-data-edit" data-testid="history-data-edit">
                          Data edit
                        </span>
                      )}
                      {explain && (
                        <span className="chip hs-explain" data-testid="history-explain">
                          {explain.analyze ? 'Analyze' : 'Explain'}
                        </span>
                      )}
                      {row.redacted && <span className="chip hs-redacted">redacted</span>}
                    </div>
                    <div className="hs-sql">{firstLine(explain ? explain.inner : row.sql_text)}</div>
                  </div>
                  {open && (
                    <div className="hs-detail" data-testid="history-detail">
                      <pre className="hs-full">{row.sql_text}</pre>
                      <div className="hs-actions">
                        <button
                          type="button"
                          className="btn small"
                          disabled={!canReRun(row) || !insertTab}
                          title={!canReRun(row) ? NOT_RE_RUNNABLE : !insertTab ? 'Open a query tab to insert a statement.' : 'Insert at the cursor of the focused editor'}
                          onClick={() => insert(row)}
                        >
                          Insert
                        </button>
                        <button
                          type="button"
                          className="btn small"
                          disabled={!canReRun(row)}
                          title={canReRun(row) ? undefined : NOT_RE_RUNNABLE}
                          onClick={() => void openInNewTab(row)}
                        >
                          Open in new tab
                        </button>
                        <button
                          type="button"
                          className="btn small"
                          disabled={!canReRun(row)}
                          title={canReRun(row) ? 'Run in a new tab; the usual guards apply' : NOT_RE_RUNNABLE}
                          onClick={() => runAgain(row)}
                        >
                          Run again
                        </button>
                        <button type="button" className="btn small" onClick={() => copy(row.sql_text)}>
                          Copy
                        </button>
                      </div>
                    </div>
                  )}
                </div>
              );
            })}
          </div>
        ))}
      </div>
      {menu && <ActionMenu label="History entry actions" x={menu.x} y={menu.y} items={menuItems(menu.row)} onClose={() => setMenu(null)} />}
    </section>
  );
}
