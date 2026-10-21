import { memo, useEffect, useLayoutEffect, useRef, useState, type KeyboardEvent, type ReactNode, type RefObject } from 'react';
import { api } from '../../api';
import { ActionMenu, type MenuEntry } from '../../components/ActionMenu';
import { addQueryToFavorites } from '../../components/favoritesActions';
import { ChevronDownIcon, MoreIcon, PlayIcon, StopIcon } from '../../components/icons';
import { formatClock } from '../../components/statusFormat';
import { useNow } from '../../lib/useNow';
import { useConnections } from '../../store/connections';
import { useExporting } from '../../store/queryExport';
import { canRun, canStop, emptyRun, useQueryRun } from '../../store/queryRun';
import { explainQuery, note as noteError, runFromEditor, stopQuery, txAction, useSelection, useStarting } from '../../store/runQuery';
import { setTxMode, txOf, useTxMode } from '../../store/txMode';
import type { EnvTag, TxMode } from '../../bindings';
import { EDIT_COMMANDS, editorOf } from './activeEditor';
import { analyzeAvailable } from './explainTree';
import { ROW_LIMITS, dialectOfConnection, useQueryDocs, type QueryDoc, type RowLimit } from '../../store/queryTabs';
import { useTabs } from '../../store/tabs';
import { useTree } from '../../store/tree';
import './toolbar.css';

/** Toolbar widths (spec §4 "Narrow groups"): under 560 px Explain / Format / Limit go into `⋯`, under 400 px Run is an icon. */
const NARROW_PX = 560;
const TINY_PX = 400;
type Width = 'wide' | 'narrow' | 'tiny';

const widthOf = (px: number): Width => (px <= 0 ? 'wide' : px < TINY_PX ? 'tiny' : px < NARROW_PX ? 'narrow' : 'wide');

/** The width class of the toolbar, from its own size (an editor group can be narrower than the window). */
function useToolbarWidth(ref: RefObject<HTMLElement | null>): Width {
  const [width, setWidth] = useState<Width>('wide');
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    setWidth(widthOf(el.getBoundingClientRect().width));
    if (typeof ResizeObserver === 'undefined') return;
    const observer = new ResizeObserver((entries) => {
      const box = entries[0]?.contentRect;
      if (box) setWidth(widthOf(box.width));
    });
    observer.observe(el);
    return () => observer.disconnect();
  }, [ref]);
  return width;
}

/** Where a menu opened from `el` goes: under its left edge. */
const below = (el: HTMLElement | null) => {
  const r = el?.getBoundingClientRect();
  return { x: r?.left ?? 0, y: (r?.bottom ?? 0) + 2 };
};

const ENV_SHORT: Record<EnvTag, string> = { production: 'PROD', staging: 'STG', local: 'LCL' };

const RUN_TIP = 'Run the statement at the cursor (Ctrl+Enter)';

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
        <ChevronDownIcon />
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
            title={analyzeOk ? 'Runs the statement and reports real timings' : ANALYZE_NEEDS}
            onClick={() => choose(true)}
          >
            Explain Analyze
          </button>
        </div>
      )}
    </span>
  );
}

const ANALYZE_NEEDS = 'Explain Analyze needs MySQL 8.0.18 or later';

/**
 * A caret button that opens an `ActionMenu` under itself. A second press on the caret closes
 * the menu (its mousedown is kept from the menu's outside-click handler, which would reopen it).
 */
function MenuButton({
  label,
  testId,
  className,
  disabled,
  items,
  checkRole,
  children,
}: {
  label: string;
  testId: string;
  className: string;
  disabled?: boolean;
  items: () => MenuEntry[];
  checkRole?: 'checkbox' | 'radio';
  children: ReactNode;
}) {
  const ref = useRef<HTMLButtonElement>(null);
  const [at, setAt] = useState<{ x: number; y: number } | null>(null);
  return (
    <>
      <button
        ref={ref}
        type="button"
        className={className}
        data-testid={testId}
        aria-label={label}
        aria-haspopup="menu"
        aria-expanded={at !== null}
        title={label}
        disabled={disabled}
        onMouseDown={(e) => {
          if (at) e.stopPropagation();
        }}
        onClick={() => setAt((v) => (v ? null : below(ref.current)))}
        onKeyDown={(e) => {
          if ((e.key === 'ArrowDown' || e.key === 'ArrowUp') && !at) {
            e.preventDefault();
            setAt(below(ref.current));
          }
        }}
      >
        {children}
      </button>
      {at && <ActionMenu label={label} x={at.x} y={at.y} items={items()} checkRole={checkRole} onClose={() => setAt(null)} />}
    </>
  );
}

/**
 * Run split button: Run (primary; "Run on PROD" as a danger outline on a Production connection)
 * or, while the tab runs or exports, Stop (danger) in the same place. Its menu holds Run all,
 * Run selection, Continue on error and Add to favorites.
 */
function RunSplit({
  doc,
  env,
  runnable,
  stoppable,
  exporting,
  hasSelection,
  why,
  iconOnly,
}: {
  doc: ToolbarDoc;
  env: EnvTag | null;
  runnable: boolean;
  stoppable: boolean;
  exporting: boolean;
  hasSelection: boolean;
  why?: string;
  iconOnly: boolean;
}) {
  const update = useQueryDocs((s) => s.update);
  const prod = env === 'production';
  const runLabel = prod ? 'Run on PROD' : 'Run';
  const tone = stoppable ? 'danger' : prod ? 'danger-outline' : 'qt-primary';
  const items = (): MenuEntry[] => [
    {
      label: 'Run all',
      hint: 'Ctrl+Shift+Enter',
      testId: 'query-run-all',
      disabled: !runnable,
      title: runnable ? 'Run every statement in the editor' : why,
      run: () => void runFromEditor(doc.id, 'all'),
    },
    {
      label: 'Run selection',
      testId: 'query-run-selection',
      disabled: !runnable || !hasSelection,
      title: !runnable ? why : hasSelection ? 'Run only the selected text' : 'Select text in the editor to run only that',
      run: () => void runFromEditor(doc.id, 'selection'),
    },
    { separator: true },
    {
      label: 'Continue on error',
      testId: 'query-continue-on-error',
      checked: doc.continueOnError,
      title: 'Keep running the next statements when one fails',
      run: () => update(doc.id, { continueOnError: !doc.continueOnError }),
    },
    { separator: true },
    {
      label: 'Add to favorites',
      hint: 'Ctrl+Shift+D',
      testId: 'query-add-favorite',
      title: 'Add the selection, or the whole query, to favorites',
      run: () => addQueryToFavorites(doc.id),
    },
  ];
  return (
    <span className="qt-split">
      {stoppable ? (
        <button
          type="button"
          className={`qt-btn qt-split-main qt-stop ${tone}`}
          data-testid="query-stop"
          aria-label={iconOnly ? 'Stop' : undefined}
          title={exporting ? 'Stop the export (Esc)' : 'Stop (Esc)'}
          onClick={() => void stopQuery(doc.id)}
        >
          <StopIcon />
          {!iconOnly && <span>Stop</span>}
        </button>
      ) : (
        <button
          type="button"
          className={`qt-btn qt-split-main qt-run ${tone}`}
          data-testid="query-run"
          aria-label={iconOnly ? runLabel : undefined}
          disabled={!runnable}
          title={runnable ? RUN_TIP : why}
          onClick={() => void runFromEditor(doc.id, 'cursor')}
        >
          <PlayIcon />
          {!iconOnly && (
            <>
              <span>{runLabel}</span>
              <kbd className="qt-kbd">Ctrl+Enter</kbd>
            </>
          )}
        </button>
      )}
      <MenuButton label="Run options" testId="query-run-menu" className={`qt-btn qt-split-caret ${tone}`} items={items}>
        <ChevronDownIcon />
      </MenuButton>
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
 * The open transaction of the tab: a warning chip "Transaction · mm:ss" with Rollback and
 * Commit (an aborted one offers Rollback only). Disabled while a statement runs.
 */
function TxControls({ tabId, busy }: { tabId: string; busy: boolean }) {
  const txState = useQueryRun((s) => (s.runs.get(tabId) ?? emptyRun).txState);
  const since = useTxMode((s) => {
    const info = txOf(s.tabs, tabId).info;
    return info?.state === 'active' ? info.since_ms : null;
  });
  const now = useNow(txState === 'active' && since !== null, since);
  if (txState === 'idle') return null;
  const why = busy ? 'A statement is running' : undefined;
  const active = txState === 'active';
  return (
    <>
      <span className={`qt-tx-chip ${active ? 'warning' : 'danger'}`} data-testid="query-tx-chip" role="status">
        {active ? `Transaction${since !== null ? ` · ${formatClock(Math.max(0, now - since))}` : ''}` : 'Transaction aborted'}
      </span>
      <button
        type="button"
        className="qt-btn"
        data-testid="query-tx-rollback"
        disabled={busy}
        title={why ?? 'Roll back the open transaction'}
        onClick={() => void txAction(tabId, 'Rollback')}
      >
        Rollback
      </button>
      {active && (
        <button
          type="button"
          className="qt-btn"
          data-testid="query-tx-commit"
          disabled={busy}
          title={why ?? 'Commit the open transaction'}
          onClick={() => void txAction(tabId, 'Commit')}
        >
          Commit
        </button>
      )}
    </>
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

/** The connection and database selects of the context picker (databases are listed while it is open). */
function ContextFields({ doc }: { doc: ToolbarDoc }) {
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
  return (
    <>
      <label className="qt-pick">
        <span>Connection</span>
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
      </label>
      <label className="qt-pick">
        <span>Database</span>
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
      </label>
    </>
  );
}

/**
 * Context pill `[ENV] connection › database`: one control that opens a picker with the
 * connection and database selects. Esc closes it and gives the focus back to the pill; an
 * outside click or the focus leaving it closes it too. The environment badge never shrinks.
 */
function ContextPill({ doc, env }: { doc: ToolbarDoc; env: EnvTag | null }) {
  const name = useConnections((s) => (doc.connectionId ? (s.summaries.get(doc.connectionId)?.config.name ?? '(deleted connection)') : null));
  const [open, setOpen] = useState(false);
  const wrap = useRef<HTMLSpanElement>(null);
  const pill = useRef<HTMLButtonElement>(null);
  const picker = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    picker.current?.querySelector<HTMLSelectElement>('select')?.focus();
    const away = (e: globalThis.MouseEvent) => {
      if (!wrap.current?.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener('mousedown', away);
    return () => document.removeEventListener('mousedown', away);
  }, [open]);

  const database = doc.database ?? 'Default database';
  const tip = name ? `${name} › ${database}${env ? ` (${env})` : ''}. Change the connection or database` : 'Choose a connection to run queries';
  return (
    <span
      className="qt-context"
      ref={wrap}
      onBlur={(e) => {
        if (open && !wrap.current?.contains(e.relatedTarget as Node | null)) setOpen(false);
      }}
    >
      <button
        ref={pill}
        type="button"
        className="qt-pill"
        data-testid="query-context"
        aria-haspopup="dialog"
        aria-expanded={open}
        title={tip}
        onClick={() => setOpen((v) => !v)}
      >
        {env && (
          <span className={`qt-env env-${env}`} data-testid="query-context-env">
            {ENV_SHORT[env]}
          </span>
        )}
        <span className="qt-pill-text">
          {name ? (
            <>
              {name}
              <span className="qt-pill-sep" aria-hidden="true">
                ›
              </span>
              <span className={doc.database ? undefined : 'qt-pill-default'}>{database}</span>
            </>
          ) : (
            'Choose a connection'
          )}
        </span>
        <ChevronDownIcon />
      </button>
      {open && (
        <div
          ref={picker}
          className="qt-popover"
          role="dialog"
          aria-label="Connection and database"
          onKeyDown={(e) => {
            if (e.key !== 'Escape') return;
            e.preventDefault();
            e.stopPropagation();
            setOpen(false);
            pill.current?.focus();
          }}
        >
          <ContextFields doc={doc} />
        </div>
      )}
    </span>
  );
}

/** The fields of a document the toolbar shows: never its text (that changes per keystroke). */
export type ToolbarDoc = Pick<QueryDoc, 'id' | 'connectionId' | 'database' | 'rowLimit' | 'continueOnError'>;

const sameToolbarDoc = (a: { doc: ToolbarDoc }, b: { doc: ToolbarDoc }) =>
  a.doc.id === b.doc.id &&
  a.doc.connectionId === b.doc.connectionId &&
  a.doc.database === b.doc.database &&
  a.doc.rowLimit === b.doc.rowLimit &&
  a.doc.continueOnError === b.doc.continueOnError;

/** Run / Stop, Explain, Format, context (connection, database), row limit and commit mode of a query tab. */
export const QueryToolbar = memo(QueryToolbarView, sameToolbarDoc);

/** Format SQL on the tab's editor (Ctrl+Shift+F there). */
function formatTab(tabId: string) {
  const editor = editorOf(tabId);
  if (!editor) return;
  editor.focus();
  editor.trigger('toolbar', EDIT_COMMANDS.formatSql, null);
}

const limitText = (n: number) => n.toLocaleString('en-US');

function QueryToolbarView({ doc }: { doc: ToolbarDoc }) {
  const summaries = useConnections((s) => s.summaries);
  const env = useConnections((s) => (doc.connectionId ? (s.summaries.get(doc.connectionId)?.config.env_tag ?? null) : null));
  const update = useQueryDocs((s) => s.update);
  const bar = useRef<HTMLDivElement>(null);
  const width = useToolbarWidth(bar);
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
  const busy = !canRun(run) || starting || exporting;
  const collapsed = width !== 'wide';

  // Under 560 px: Explain, Format and the row limit, in that order.
  const moreItems = (): MenuEntry[] => [
    {
      label: 'Explain',
      hint: 'Ctrl+Shift+E',
      testId: 'query-more-explain',
      disabled: !runnable,
      title: why,
      run: () => void explainQuery(doc.id, false),
    },
    {
      label: 'Explain Analyze',
      testId: 'query-more-explain-analyze',
      disabled: !runnable || !analyzeOk,
      title: !runnable ? why : analyzeOk ? 'Runs the statement and reports real timings' : ANALYZE_NEEDS,
      run: () => void explainQuery(doc.id, true),
    },
    { label: 'Format SQL', hint: 'Ctrl+Shift+F', testId: 'query-more-format', run: () => formatTab(doc.id) },
    { separator: true },
    ...ROW_LIMITS.map((n) => ({
      label: `Limit ${limitText(n)}`,
      checked: doc.rowLimit === n,
      run: () => update(doc.id, { rowLimit: n }),
    })),
  ];

  return (
    <div className={`query-toolbar w-${width}`} role="toolbar" aria-label="Query" ref={bar}>
      <RunSplit
        doc={doc}
        env={env}
        runnable={runnable}
        stoppable={stoppable}
        exporting={exporting}
        hasSelection={hasSelection}
        why={why}
        iconOnly={width === 'tiny'}
      />
      {collapsed ? (
        <MenuButton label="More query actions" testId="query-more" className="qt-btn qt-icon" items={moreItems} checkRole="radio">
          <MoreIcon />
        </MenuButton>
      ) : (
        <>
          <ExplainSplit tabId={doc.id} runnable={runnable} analyzeOk={analyzeOk} why={why} />
          <button type="button" className="qt-btn" data-testid="query-format" title="Format SQL (Ctrl+Shift+F)" onClick={() => formatTab(doc.id)}>
            Format
          </button>
        </>
      )}
      <span className="qt-sep" aria-hidden="true" />
      <ContextPill doc={doc} env={env} />
      {!collapsed && (
        <label className="qt-field" title="Most rows a result keeps">
          <span>Limit</span>
          <select
            className="qt-select qt-limit"
            data-testid="query-row-limit"
            aria-label="Row limit"
            value={doc.rowLimit}
            onChange={(e) => update(doc.id, { rowLimit: Number(e.target.value) as RowLimit })}
          >
            {ROW_LIMITS.map((n) => (
              <option key={n} value={n}>
                {limitText(n)}
              </option>
            ))}
          </select>
        </label>
      )}
      <span className="qt-tx">
        <CommitMode tabId={doc.id} busy={busy} />
        <TxControls tabId={doc.id} busy={busy} />
      </span>
    </div>
  );
}
