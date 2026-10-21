import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it } from 'vitest';
import type { PlanStmtView, RunEvent } from '../bindings';
import { emptyRun, reduceRun, startRun, useQueryRun, type TabRun } from '../store/queryRun';
import { useQueryDocs } from '../store/queryTabs';
import { useDialogs } from '../store/dialogs';
import { useTabs } from '../store/tabs';
import { ResultPanel } from '../tabs/query/ResultPanel';
import { ids, mockBackend, resetStores } from '../test/backend';
import { DialogHost } from './DialogHost';
import { openExportDialog, queryAllUnavailable, queryExportRequest, safeFileName, shownResult } from './resultExport';

const stmt = (index: number, kind: PlanStmtView['kind'], verdict: PlanStmtView['verdict'] = 'allow'): PlanStmtView => ({
  index,
  range: [0, 8],
  kind,
  verdict,
  preview: `S${index}`,
});

const plan = (stmts: PlanStmtView[]) => ({
  plan_id: 'p1',
  stmts,
  dialect: { engine: 'mysql' as const },
  connection_name: 'c',
  env_tag: null,
  read_only: false,
  tx_state: 'idle' as const,
});

const col = { name: 'a', db_type: 'int', logical: 'Integer' as const, nullable: true, origin_table: null, origin_column: null };

function play(stmts: PlanStmtView[], truncated = false): TabRun {
  const events: RunEvent[] = [
    { t: 'statement_start', index: 0, range: [0, 8] },
    { t: 'result_start', result_id: 1, index: 0, columns: [col] },
    { t: 'result_stored', result_id: 1, token: 'tok-1' },
    { t: 'rows_stored', result_id: 1, start: 0, row_count: 2 },
    { t: 'result_end', result_id: 1, row_count: 2, affected: null, truncated, elapsed_ms: 1 },
    { t: 'done', stop: null, elapsed_ms: 1 },
  ];
  let run = startRun(emptyRun, plan(stmts), 0);
  events.forEach((ev, i) => (run = reduceRun(run, ev, i + 1)));
  return run;
}

/** A query tab on the prod connection with `run` as its last run. */
function queryTab(run: TabRun): string {
  const id = useQueryDocs.getState().newQuery(ids.prod, 'shop');
  useQueryRun.setState({ runs: new Map([[id, { ...run, origin: { connectionId: ids.prod, database: 'shop' } }]]) });
  return id;
}

beforeEach(() => {
  resetStores();
  mockBackend({
    query_result_page: () => ({
      rows: [[{ t: 'Int', v: 1 }], [{ t: 'Int', v: 2 }]],
      truncated_cells: [],
      offset: 0,
      committed_rows: 2,
      done: true,
      bytes: 16,
    }),
  });
  shownResult.clear();
});

describe('result export requests', () => {
  it('a read result can be read again; other results export their loaded rows', () => {
    const read = play([stmt(0, 'read')]);
    expect(queryAllUnavailable(read, read.results[0]!, true)).toBeNull();
    expect(queryAllUnavailable(read, read.results[0]!, false)).toMatch(/no longer linked/);
    const write = play([stmt(0, 'write')]);
    expect(queryAllUnavailable(write, write.results[0]!, true)).toBe('Only the result of a read statement can be read again.');
    const confirm = play([stmt(0, 'read', 'confirm')]);
    expect(queryAllUnavailable(confirm, confirm.results[0]!, true)).toMatch(/needs confirmation/);
  });

  it('queryExportRequest describes the shown result', () => {
    const id = queryTab(play([stmt(0, 'read')], true));
    const r = queryExportRequest(id, 0)!;
    expect(r).toMatchObject({
      title: 'Export result 1',
      engine: 'mysql',
      loadedCount: 2,
      all: { kind: 'query', connectionId: ids.prod, token: 'tok-1' },
      allUnavailable: null,
    });
    expect(r.loadedNote).toBe('Only the first 2 rows were fetched.');
    expect(queryExportRequest(id, 5)).toBeNull();
  });

  it('file names lose the characters Windows refuses', () => {
    expect(safeFileName('a/b:c*?"<>|d')).toBe('a_b_c______d');
    expect(safeFileName('  ')).toBe('export');
    expect(safeFileName('đơn_hàng')).toBe('đơn_hàng');
  });

  it('the result panel Export… button opens the dialog for its result', async () => {
    const user = userEvent.setup();
    const id = queryTab(play([stmt(0, 'read')]));
    render(
      <>
        <ResultPanel tabId={id} />
        <DialogHost />
      </>,
    );
    await user.click(screen.getByTestId('result-export'));
    expect(useDialogs.getState().request?.kind).toBe('exportResult');
    expect(await screen.findByRole('dialog', { name: 'Export result 1' })).toBeInTheDocument();
    expect(screen.getByRole('radio', { name: 'Exact stored result (2 rows)' })).toBeChecked();
  });

  it('openExportDialog (menu hook) exports what the active query tab shows', () => {
    const id = queryTab(play([stmt(0, 'read')]));
    useTabs.getState().activate(id);
    openExportDialog();
    const request = useDialogs.getState().request;
    expect(request?.kind === 'exportResult' && request.request.title).toBe('Export result 1');
    // Nothing to export: no dialog.
    useDialogs.getState().closeRequest();
    useQueryRun.setState({ runs: new Map() });
    openExportDialog();
    expect(useDialogs.getState().request).toBeNull();
  });
});
