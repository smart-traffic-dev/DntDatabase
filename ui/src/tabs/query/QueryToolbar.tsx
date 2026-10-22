import { memo, useEffect, useLayoutEffect, useRef, useState, type KeyboardEvent, type ReactNode } from 'react';
import { api } from '../../api';
import { ActionMenu, type MenuEntry } from '../../components/ActionMenu';
import { addQueryToFavorites } from '../../components/favoritesActions';
import { focusFocusedGroup } from '../../components/focusGroup';
import { CheckIcon, ChevronDownIcon, DatabaseIcon, ExplainIcon, MoreIcon, PlayIcon, RollbackIcon, StopIcon } from '../../components/icons';
import { formatClock } from '../../components/statusFormat';
import { useNow } from '../../lib/useNow';
import { useConnections } from '../../store/connections';
import { useExporting } from '../../store/queryExport';
import { canRun, canStop, emptyRun, runElapsedMs, useQueryRun } from '../../store/queryRun';
import { explainQuery, note as noteError, runFromEditor, stopQuery, txAction, useSelection, useStarting } from '../../store/runQuery';
import { setTxMode, txOf, useTxMode } from '../../store/txMode';
import type { EnvTag, TxMode } from '../../bindings';
import { EDIT_COMMANDS, editorOf } from './activeEditor';
import { ANALYZE_NEEDS, useRunGate } from './runGate';
import { ROW_LIMITS, dialectOfConnection, useQueryDocs, type QueryDoc } from '../../store/queryTabs';
import { MAX_GROUPS, useTabs } from '../../store/tabs';
import { useTree } from '../../store/tree';
import { ENV_SHORT, belowLeft, belowRight, useStripWidth } from '../stripWidth';
import './toolbar.css';

const RUN_TIP = 'Run the selection, or every statement (Ctrl+R)';

/**
 * Explain (icon) and its menu (Explain, Explain Analyze). The menu takes focus when it opens;
 * arrows, Home and End move between its enabled items; Escape closes it and gives focus back to
 * the caret; an outside click or focus leaving it closes it.
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
        className="ta-btn"
        data-testid="query-explain"
        aria-label="Explain"
        disabled={!runnable}
        title={runnable ? 'Explain (Ctrl+Shift+E)' : why}
        onClick={() => void explainQuery(tabId, false)}
      >
        <ExplainIcon />
        <span className="ta-label">Explain</span>
      </button>
      <button
        ref={caret}
        type="button"
        className="ta-caret"
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


/**
 * A button that opens an `ActionMenu` under itself, right-aligned with it by default (`align`
 * "left" for a button at the toolbar's left side). A second press closes the menu (its mousedown
 * is kept from the menu's outside-click handler, which would reopen it).
 */
export function MenuButton({
  label,
  title,
  testId,
  className,
  disabled,
  align = 'right',
  items,
  checkRole,
  children,
}: {
  label: string;
  /** Tooltip; the label by default. */
  title?: string;
  testId?: string;
  className: string;
  disabled?: boolean;
  align?: 'left' | 'right';
  items: () => MenuEntry[];
  checkRole?: 'checkbox' | 'radio';
  children: ReactNode;
}) {
  const ref = useRef<HTMLButtonElement>(null);
  const [at, setAt] = useState<{ x: number; y: number } | null>(null);
  // A split's ▾ starts its menu under the split's main button, not under the narrow caret.
  const place = () => (align === 'left' ? belowLeft(ref.current?.parentElement ?? ref.current) : belowRight(ref.current));
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
        title={title ?? label}
        disabled={disabled}
        onMouseDown={(e) => {
          if (at) e.stopPropagation();
        }}
        onClick={() => setAt((v) => (v ? null : place()))}
        onKeyDown={(e) => {
          if ((e.key === 'ArrowDown' || e.key === 'ArrowUp') && !at) {
            e.preventDefault();
            setAt(place());
          }
        }}
      >
        {children}
      </button>
      {at && <ActionMenu label={label} x={at.x} y={at.y} items={items()} checkRole={checkRole} onClose={() => setAt(null)} />}
    </>
  );
}

/** Stop ignores clicks this long after it replaced Run (a double-click on Run must not cancel). */
const STOP_GUARD_MS = 400;

/**
 * Run (the selection, else every statement; red on a Production connection) or, while the tab
 * runs or exports, Stop in the same
 * element and slot with the elapsed time inside it. Its ▾ menu holds Run statement, Run all,
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
}: {
  doc: ToolbarDoc;
  env: EnvTag | null;
  runnable: boolean;
  stoppable: boolean;
  exporting: boolean;
  hasSelection: boolean;
  why?: string;
}) {
  const update = useQueryDocs((s) => s.update);
  const run = useQueryRun((s) => s.runs.get(doc.id) ?? emptyRun);
  const startedAt = run.phase.kind === 'running' ? run.phase.startedAt : (run.startedAt ?? null);
  const now = useNow(stoppable, startedAt);
  const elapsed = stoppable ? runElapsedMs(run, now) : null;
  const stopShownAt = useRef(0);
  useLayoutEffect(() => {
    if (stoppable) stopShownAt.current = Date.now();
  }, [stoppable]);
  const prod = env === 'production';
  const runLabel = prod ? 'Run on PROD' : 'Run';
  const tone = stoppable ? 'danger' : prod ? 'danger-outline' : 'qt-primary';
  const items = (): MenuEntry[] => [
    {
      label: 'Run statement',
      hint: 'Ctrl+Enter',
      testId: 'query-run-statement',
      disabled: !runnable,
      title: runnable ? 'Run the statement at the cursor' : why,
      run: () => void runFromEditor(doc.id, 'cursor'),
    },
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
          className={`ta-btn qt-stop ${tone}`}
          data-testid="query-stop"
          aria-label="Stop"
          title={exporting ? 'Stop the export (Esc)' : 'Stop (Esc)'}
          onClick={(e) => {
            // The second click of a double-click on Run lands here once Run became Stop: ignore it,
            // and any click in the first moments after Stop appeared.
            if (e.detail > 1 || Date.now() - stopShownAt.current < STOP_GUARD_MS) return;
            void stopQuery(doc.id);
          }}
        >
          <StopIcon />
          <span className="ta-label">Stop</span>
          {elapsed !== null && (
            <span className="ta-elapsed" data-testid="query-elapsed">
              {formatClock(elapsed)}
            </span>
          )}
        </button>
      ) : (
        <button
          type="button"
          className={`ta-btn qt-run ${tone}`}
          data-testid="query-run"
          aria-label={runLabel}
          disabled={!runnable}
          title={runnable ? RUN_TIP : why}
          onClick={() => void runFromEditor(doc.id, 'script')}
        >
          <PlayIcon />
          <span className="ta-label">{runLabel}</span>
        </button>
      )}
      <MenuButton label="Run options" testId="query-run-menu" className="ta-caret" align="left" items={items}>
        <ChevronDownIcon />
      </MenuButton>
    </span>
  );
}

const MANUAL_TIP =
  'Each statement that changes data starts a transaction you commit or roll back yourself. Procedures that commit internally need Auto mode.';

/**
 * `Auto | Manual` commit mode of the tab (two radio items of `⋯`). Locked while a run is active
 * and while a Manual transaction is open (or lost, until acknowledged): Commit or Rollback first.
 * When the backend refuses a switch the error goes to Messages and the mode stays.
 */
function useCommitMode(tabId: string, busy: boolean) {
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

  const item = (value: TxMode, tip: string): MenuEntry => ({
    label: `${value} commit`,
    testId: `query-tx-${value.toLowerCase()}`,
    checked: mode === value,
    disabled: locked,
    title: lockWhy ?? tip,
    run: () => void choose(value),
  });
  return [item('Auto', 'Every statement commits as it runs'), item('Manual', MANUAL_TIP)];
}

/**
 * The open transaction of the tab: an amber chip "Tx mm:ss" with Commit and Rollback icons (an
 * aborted one offers Rollback only). Disabled while a statement runs. Nothing while none is open.
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
  const age = since !== null ? formatClock(Math.max(0, now - since)) : null;
  return (
    <>
      <span className="ta-sep" aria-hidden="true" />
      <span
        className={`qt-tx-chip ${active ? 'warning' : 'danger'}`}
        data-testid="query-tx-chip"
        role="status"
        title={active ? `Open transaction${age ? `, ${age}` : ''}: commit or roll it back` : 'The transaction failed: roll it back'}
      >
        {active ? `Tx${age ? ` ${age}` : ''}` : 'Tx aborted'}
      </span>
      {active && (
        <button
          type="button"
          className="ta-btn ta-commit"
          data-testid="query-tx-commit"
          aria-label="Commit"
          disabled={busy}
          title={why ?? 'Commit the open transaction'}
          onClick={() => void txAction(tabId, 'Commit')}
        >
          <CheckIcon />
        </button>
      )}
      <button
        type="button"
        className="ta-btn ta-rollback"
        data-testid="query-tx-rollback"
        aria-label="Rollback"
        disabled={busy}
        title={why ?? 'Roll back the open transaction'}
        onClick={() => void txAction(tabId, 'Rollback')}
      >
        <RollbackIcon />
      </button>
    </>
  );
}

/**
 * Database names of an open connection (`null`: none). The tree only lists them once its
 * connection node is expanded; the picker must offer them on an already-open connection too.
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

/** Space kept between a shifted picker and the group's (or window's) edges. */
const POPOVER_GAP = 8;

/**
 * Context chip `[ENV] database` (connection, server and environment in the tooltip): one control
 * that opens a picker with the connection and database selects. Esc closes it and gives the focus
 * back to the chip; an outside click or the focus leaving it closes it too. In a narrow group the
 * chip is the badge only.
 */
function ContextChip({ doc, env, narrow }: { doc: ToolbarDoc; env: EnvTag | null; narrow: boolean }) {
  const conn = useConnections((s) => (doc.connectionId ? s.summaries.get(doc.connectionId)?.config : undefined));
  const name = doc.connectionId ? (conn?.name ?? '(deleted connection)') : null;
  const [open, setOpen] = useState(false);
  const wrap = useRef<HTMLSpanElement>(null);
  const chip = useRef<HTMLButtonElement>(null);
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

  // The picker opens under the chip, right-aligned with it; when that runs past the group's left
  // edge (or the window's) it shifts right to fit, keeping a small gap.
  useLayoutEffect(() => {
    const el = picker.current;
    if (!open || !el) return;
    const bound = el.closest('.editor-group')?.getBoundingClientRect();
    const left = bound ? bound.left : 0;
    const box = el.getBoundingClientRect();
    const under = left + POPOVER_GAP - box.left;
    if (under > 0) el.style.transform = `translateX(${under}px)`;
  }, [open]);

  const database = doc.database ?? 'Default database';
  const server = conn ? ` (${conn.host}:${conn.port})` : '';
  const tip = name
    ? `${name}${server} › ${database}${env ? ` · ${env}` : ''}. Change the connection or database`
    : 'Choose a connection to run queries';
  const text = name ? database : 'Choose a connection';
  return (
    <span
      className="qt-context"
      ref={wrap}
      onBlur={(e) => {
        if (open && !wrap.current?.contains(e.relatedTarget as Node | null)) setOpen(false);
      }}
    >
      <button
        ref={chip}
        type="button"
        className="ta-chip"
        data-testid="query-context"
        aria-haspopup="dialog"
        aria-expanded={open}
        aria-label={narrow ? `Connection: ${name ? `${name} › ${database}` : 'none'}` : undefined}
        title={tip}
        onClick={() => setOpen((v) => !v)}
      >
        {env && (
          <span className={`qt-env env-${env}`} data-testid="query-context-env">
            {ENV_SHORT[env]}
          </span>
        )}
        {narrow ? (
          !env && <DatabaseIcon />
        ) : (
          <>
            <span className={`ta-chip-text${doc.database ? '' : ' ta-chip-default'}`}>{text}</span>
            <span className="ta-chip-caret" aria-hidden="true">
              <ChevronDownIcon />
            </span>
          </>
        )}
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
            chip.current?.focus();
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

/**
 * The query tab's toolbar, on its own row under the group's tab strip: context chip, Run / Stop ▾,
 * Explain ▾, open transaction (only while open) and, at the right end, `⋯` (Format, Limit,
 * Auto / Manual commit, Open in new group).
 */
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
  const env = useConnections((s) => (doc.connectionId ? (s.summaries.get(doc.connectionId)?.config.env_tag ?? null) : null));
  const update = useQueryDocs((s) => s.update);
  const bar = useRef<HTMLDivElement>(null);
  const width = useStripWidth(bar);
  const narrow = width === 'narrow';
  const run = useQueryRun((s) => s.runs.get(doc.id) ?? emptyRun);
  // A tab without a (still stored) connection stays disabled; a run locks the toolbar until it ends.
  const hasSelection = useSelection((s) => s.ids.has(doc.id));
  const starting = useStarting((s) => s.ids.has(doc.id));
  // "All rows" of a result being exported runs on this tab's session: the tab is busy.
  const exporting = useExporting((s) => s.runs.has(doc.id));
  const canSplit = useTabs((s) => s.groups.length < MAX_GROUPS);
  const { runnable, why, analyzeOk } = useRunGate(doc.id, doc.connectionId);
  const stoppable = canStop(run) || exporting;
  const busy = !canRun(run) || starting || exporting;
  const commitItems = useCommitMode(doc.id, busy);

  const moreItems = (): MenuEntry[] => [
    // Under 360 px Explain lives here.
    ...(narrow
      ? ([
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
          { separator: true },
        ] satisfies MenuEntry[])
      : []),
    { label: 'Format SQL', hint: 'Ctrl+Shift+F', testId: 'query-format', run: () => formatTab(doc.id) },
    { separator: true },
    ...ROW_LIMITS.map((n) => ({
      label: `Display limit ${limitText(n)}`,
      testId: `query-row-limit-${n}`,
      title: 'Rows kept for display; the database may still finish producing the result',
      checked: doc.rowLimit === n,
      run: () => update(doc.id, { rowLimit: n }),
    })),
    { separator: true },
    ...commitItems,
    { separator: true },
    {
      label: 'Open in new group',
      hint: 'Ctrl+\\',
      testId: 'query-open-in-group',
      disabled: !canSplit,
      title: canSplit ? 'Move this tab into a new group on the right' : `At most ${MAX_GROUPS} groups`,
      run: () => {
        useTabs.getState().splitTab(doc.id, 'right');
        focusFocusedGroup();
      },
    },
  ];

  return (
    <div className={`tab-actions w-${width}`} role="toolbar" aria-label="Query" ref={bar}>
      <ContextChip doc={doc} env={env} narrow={narrow} />
      <span className="ta-sep" aria-hidden="true" />
      <RunSplit doc={doc} env={env} runnable={runnable} stoppable={stoppable} exporting={exporting} hasSelection={hasSelection} why={why} />
      {!narrow && <ExplainSplit tabId={doc.id} runnable={runnable} analyzeOk={analyzeOk} why={why} />}
      <TxControls tabId={doc.id} busy={busy} />
      <span className="ta-fill" />
      <MenuButton
        label="More query actions"
        title="Format, limit, commit mode, open in new group"
        testId="query-more"
        className="ta-btn"
        items={moreItems}
        checkRole="radio"
      >
        <MoreIcon />
      </MenuButton>
    </div>
  );
}
