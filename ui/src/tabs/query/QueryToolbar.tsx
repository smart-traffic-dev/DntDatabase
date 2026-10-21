import { memo, useEffect, useRef, useState, type KeyboardEvent } from 'react';
import { api } from '../../api';
import { addQueryToFavorites } from '../../components/favoritesActions';
import { useConnections } from '../../store/connections';
import { useExporting } from '../../store/queryExport';
import { canRun, canStop, emptyRun, useQueryRun } from '../../store/queryRun';
import { explainQuery, note as noteError, runFromEditor, stopQuery, useSelection, useStarting } from '../../store/runQuery';
import { setTxMode, txOf, useTxMode } from '../../store/txMode';
import type { TxMode } from '../../bindings';
import { analyzeAvailable } from './explainTree';
import { ROW_LIMITS, dialectOfConnection, useQueryDocs, type QueryDoc, type RowLimit } from '../../store/queryTabs';
import { useTabs } from '../../store/tabs';
import { useTree } from '../../store/tree';
import './toolbar.css';

/**
 * Explain and its menu (Explain, Explain Analyze). The menu takes focus when it opens; arrows,
 * Home and End move between its enabled items; Escape closes it and gives focus back to the caret;
 * an outside click or focus leaving it closes it.
 */
function ExplainSplit({ tabId, runnable, analyzeOk, why }: { tabId: string; runnable: boolean; analyzeOk: boolean; why?: string }) {
  const [open, setOpen] = useState(false);
  const split = useRef<HTMLSpanElement>(null);
  const caret = useRef<HTMLButtonElement>(null);
  const menu = useRef<HTMLDivElement>(null);

  const enabledItems = () => [...(menu.current?.querySelectorAll<HTMLButtonElement>('[role="menuitem"]:not(:disabled)') ?? [])];

  useEffect(() => {
    if (!open) return;
    enabledItems()[0]?.focus();
    const away = (e: globalThis.MouseEvent) => {
      if (!split.current?.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener('mousedown', away);
    return () => document.removeEventListener('mousedown', away);
  }, [open]);

  // A run that starts (or a lost connection) disables Explain: the menu goes with it.
  if (open && !runnable) setOpen(false);

  const close = (refocus: boolean) => {
    setOpen(false);
    if (refocus) caret.current?.focus();
  };

  const onMenuKey = (e: KeyboardEvent) => {
    const items = enabledItems();
    const at = items.indexOf(document.activeElement as HTMLButtonElement);
    const go = (i: number) => {
      e.preventDefault();
      items[(i + items.length) % items.length]?.focus();
    };
    switch (e.key) {
      case 'ArrowDown':
        return go(at + 1);
      case 'ArrowUp':
        return go(at < 0 ? -1 : at - 1);
      case 'Home':
        return go(0);
      case 'End':
        return go(-1);
      case 'Escape':
        e.preventDefault();
        return close(true);
      case 'Tab':
        return close(false);
    }
  };

  const choose = (analyze: boolean) => {
    close(true);
    void explainQuery(tabId, analyze);
  };

  return (
    <span
      className="qt-split"
      ref={split}
      onBlur={(e) => {
        if (open && !split.current?.contains(e.relatedTarget as Node | null)) setOpen(false);
      }}
    >
      <button
        type="button"
        className="qt-btn qt-split-main"
        data-testid="query-explain"
        disabled={!runnable}
        title={runnable ? 'Explain (Ctrl+Shift+E)' : why}
        onClick={() => void explainQuery(tabId, false)}
      >
        Explain
      </button>
      <button
        ref={caret}
        type="button"
        className="qt-btn qt-split-caret"
        data-testid="query-explain-menu"
        aria-label="Explain options"
        aria-haspopup="menu"
        aria-expanded={open}
        aria-controls={open ? `qt-explain-menu-${tabId}` : undefined}
        disabled={!runnable}
        title={runnable ? 'Explain options' : why}
        onClick={() => setOpen((v) => !v)}
        onKeyDown={(e) => {
          if (e.key === 'Escape') setOpen(false);
          else if ((e.key === 'ArrowDown' || e.key === 'ArrowUp') && !open) {
            e.preventDefault();
            setOpen(true);
          }
        }}
      >
        ▾
      </button>
      {open && (
        <div
          ref={menu}
          id={`qt-explain-menu-${tabId}`}
          className="qt-menu"
          role="menu"
          aria-label="Explain options"
          onKeyDown={onMenuKey}
        >
          <button type="button" role="menuitem" className="qt-menu-item" data-testid="query-explain-plain" tabIndex={-1} onClick={() => choose(false)}>
            Explain
          </button>
          <button
            type="button"
            role="menuitem"
            className="qt-menu-item"
            data-testid="query-explain-analyze"
            tabIndex={-1}
            disabled={!analyzeOk}
            title={analyzeOk ? 'Runs the statement and reports real timings' : 'Explain Analyze needs MySQL 8.0.18 or later'}
            onClick={() => choose(true)}
          >
            Explain Analyze
          </button>
        </div>
      )}
    </span>
  );
}

const MANUAL_TIP =
  'Each statement that changes data starts a transaction you commit or roll back yourself. Procedures that commit internally need Auto mode.';

/**
 * `Auto | Manual` commit mode of the tab. Locked while a run is active and while a Manual
 * transaction is open (or lost, until acknowledged): Commit or Rollback first. When the backend
 * refuses a switch the error goes to Messages and the mode stays.
 */
function CommitMode({ tabId, busy }: { tabId: string; busy: boolean }) {
  const mode = useTxMode((s) => txOf(s.tabs, tabId).mode);
  const lost = useTxMode((s) => txOf(s.tabs, tabId).lost !== null);
  const txState = useQueryRun((s) => (s.runs.get(tabId) ?? emptyRun).txState);
  const pending = useRef(false);
  const holding = mode === 'Manual' && (txState !== 'idle' || lost);
  const locked = busy || holding;
  const lockWhy = holding ? 'Commit or roll back the open transaction first' : busy ? 'A statement is running' : undefined;

  const choose = async (next: TxMode) => {
    if (next === mode || locked || pending.current) return;
    pending.current = true;
    try {
      const error = await setTxMode(tabId, next);
      if (error) noteError(tabId, error.message, error);
    } finally {
      pending.current = false;
    }
  };

  const option = (value: TxMode, tip: string) => (
    <button
      type="button"
      role="radio"
      aria-checked={mode === value}
      className={`qt-btn qt-seg${mode === value ? ' on' : ''}`}
      data-testid={`query-tx-${value.toLowerCase()}`}
      disabled={locked}
      title={lockWhy ?? tip}
      onClick={() => void choose(value)}
    >
      {value}
    </button>
  );

  return (
    <span className="qt-segs" role="radiogroup" aria-label="Commit mode">
      {option('Auto', 'Every statement commits as it runs')}
      {option('Manual', MANUAL_TIP)}
    </span>
  );
}

/**
 * Database names of an open connection (`null`: none). The tree only lists them once its
 * connection node is expanded; the toolbar must offer them on an already-open connection too.
 * A failed listing leaves the list empty: the tree's names still show.
 */
function useOpenDatabases(connectionId: string | null, showSystem: boolean): string[] {
  const [listed, setListed] = useState<{ key: string; names: string[] } | null>(null);
  const key = connectionId ? `${connectionId}:${showSystem}` : null;
  useEffect(() => {
    if (!connectionId || !key) return;
    let live = true;
    api
      .listDatabases(connectionId, showSystem)
      .then((list) => {
        if (live && Array.isArray(list)) setListed({ key, names: list.map((d) => d.name) });
      })
      .catch(() => {});
    return () => {
      live = false;
    };
  }, [connectionId, showSystem, key]);
  return listed && listed.key === key ? listed.names : [];
}

/** The fields of a document the toolbar shows: never its text (that changes per keystroke). */
export type ToolbarDoc = Pick<QueryDoc, 'id' | 'connectionId' | 'database' | 'rowLimit' | 'continueOnError'>;

const sameToolbarDoc = (a: { doc: ToolbarDoc }, b: { doc: ToolbarDoc }) =>
  a.doc.id === b.doc.id &&
  a.doc.connectionId === b.doc.connectionId &&
  a.doc.database === b.doc.database &&
  a.doc.rowLimit === b.doc.rowLimit &&
  a.doc.continueOnError === b.doc.continueOnError;

/** Run / Stop / Explain, connection, database, row limit and continue-on-error of a query tab. */
export const QueryToolbar = memo(QueryToolbarView, sameToolbarDoc);

function QueryToolbarView({ doc }: { doc: ToolbarDoc }) {
  const summaries = useConnections((s) => s.summaries);
  const open = useConnections((s) => (doc.connectionId ? s.info.has(doc.connectionId) : false));
  const nodes = useTree((s) => s.nodes);
  const showSystem = useTree((s) => s.showSystem);
  const update = useQueryDocs((s) => s.update);
  const fetched = useOpenDatabases(open ? doc.connectionId : null, showSystem);

  // The databases of the connection: the ones the tree has loaded, plus the ones listed for an
  // open connection whose node was never expanded.
  const prefix = doc.connectionId ? `conn:${doc.connectionId}/db:` : null;
  const databases = new Set<string>(fetched);
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
  const run = useQueryRun((s) => s.runs.get(doc.id) ?? emptyRun);
  // A tab without a (still stored) connection stays disabled; a run locks the toolbar until it ends.
  const hasSelection = useSelection((s) => s.ids.has(doc.id));
  const starting = useStarting((s) => s.ids.has(doc.id));
  // "All rows" of a result being exported runs on this tab's session: the tab is busy.
  const exporting = useExporting((s) => s.runs.has(doc.id));
  const runnable = !!doc.connectionId && summaries.has(doc.connectionId) && canRun(run) && !starting && !exporting;
  const stoppable = canStop(run) || exporting;
  const why = runnable
    ? undefined
    : !doc.connectionId
      ? 'Choose a connection to run queries'
      : !summaries.has(doc.connectionId)
        ? 'The connection of this tab no longer exists'
        : exporting
          ? 'An export is reading this tab’s result — stop it first'
          : 'A statement is running — stop it first';
  const analyzeOk = analyzeAvailable(useConnections((s) => (doc.connectionId ? s.info.get(doc.connectionId) : undefined)));

  return (
    <div className="query-toolbar" role="toolbar" aria-label="Query">
      <button
        type="button"
        className="qt-btn qt-run"
        data-testid="query-run"
        disabled={!runnable}
        title={runnable ? 'Run the statement at the cursor (Ctrl+Enter)' : why}
        onClick={() => void runFromEditor(doc.id, 'cursor')}
      >
        Run
        <kbd className="qt-kbd">Ctrl+Enter</kbd>
      </button>
      <button
        type="button"
        className="qt-btn"
        data-testid="query-run-all"
        disabled={!runnable}
        title={runnable ? 'Run every statement in the editor (Ctrl+Shift+Enter)' : why}
        onClick={() => void runFromEditor(doc.id, 'all')}
      >
        Run all
        <kbd className="qt-kbd">Ctrl+Shift+Enter</kbd>
      </button>
      <button
        type="button"
        className="qt-btn"
        data-testid="query-run-selection"
        disabled={!runnable || !hasSelection}
        title={!runnable ? why : hasSelection ? 'Run only the selected text' : 'Select text in the editor to run only that'}
        onClick={() => void runFromEditor(doc.id, 'selection')}
      >
        Run selection
      </button>
      <button
        type="button"
        className="qt-btn"
        data-testid="query-stop"
        disabled={!stoppable}
        title={exporting ? 'Stop the export (Esc)' : stoppable ? 'Stop (Esc)' : 'Nothing is running'}
        onClick={() => void stopQuery(doc.id)}
      >
        Stop
      </button>
      <ExplainSplit tabId={doc.id} runnable={runnable} analyzeOk={analyzeOk} why={why} />
      <button
        type="button"
        className="qt-btn"
        data-testid="query-add-favorite"
        title="Add the selection, or the whole query, to favorites (Ctrl+Shift+D)"
        onClick={() => addQueryToFavorites(doc.id)}
      >
        Add to favorites
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
        title={!doc.connectionId ? 'Choose a connection first' : open ? undefined : 'Open the connection to list all its databases'}
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
      <CommitMode tabId={doc.id} busy={!canRun(run) || starting || exporting} />
    </div>
  );
}
