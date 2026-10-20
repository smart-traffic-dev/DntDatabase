import { act, render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import type { ColumnMeta, ExportProgress, ObjectRef, Value } from '../bindings';
import { exportRunOf, setExporting } from '../store/queryExport';
import { useQueryRun } from '../store/queryRun';
import { stopQuery } from '../store/runQuery';
import { callsOf, fail, ids, mockBackend, resetStores } from '../test/backend';
import { ResultExportDialog } from './ResultExportDialog';
import { countPartialBinaries, type ExportRequest } from './resultExport';

const col = (name: string): ColumnMeta => ({ name, db_type: 'int', logical: 'Integer', nullable: true, origin_table: null, origin_column: null });
const rows: Value[][] = [
  [{ t: 'Int', v: 1 }, { t: 'Text', v: 'a' }],
  [{ t: 'Int', v: 2 }, { t: 'Null' }],
];
const obj: ObjectRef = { database: 'shop', schema: null, name: 'orders', kind: 'Table' };

function request(over: Partial<ExportRequest> = {}): ExportRequest {
  return {
    title: 'Export orders',
    engine: 'mysql',
    baseName: 'orders',
    tableName: 'orders',
    columns: [col('id'), col('name')],
    rows: () => rows,
    loadedCount: 2,
    loadedNote: null,
    partialBinaries: 0,
    all: { kind: 'table', connectionId: ids.prod, obj, sort: [{ column: 'id', dir: 'desc' }], filter: null },
    allUnavailable: null,
    ...over,
  };
}

const summary = (n: number, path: string, partial = 0) => ({ rows: n, path, cut_cells: 0, partial_binaries: partial, elapsed_ms: 3 });

beforeEach(() => {
  resetStores();
});

afterEach(() => {
  setExporting('tab-q', null);
});

describe('ResultExportDialog', () => {
  it('exports the loaded rows as CSV with the chosen options, then offers Open folder', async () => {
    const user = userEvent.setup();
    mockBackend({
      export_pick_path: () => 'C:\\out\\orders.csv',
      export_rows: (a) => summary(2, a.path as string),
      reveal_export: () => null,
    });
    render(<ResultExportDialog request={request()} onClose={() => {}} />);
    expect(screen.getByLabelText('Format')).toHaveValue('csv');
    await user.selectOptions(screen.getByLabelText('Delimiter'), 'Semicolon (;)');
    await user.click(screen.getByRole('checkbox', { name: 'UTF-8 BOM (for Excel)' }));
    await user.click(screen.getByRole('checkbox', { name: 'Quote all fields' }));
    expect(screen.getByRole('checkbox', { name: 'Protect against formula injection' })).not.toBeChecked();
    expect(screen.getByText(/NULL is an empty field; an empty string is written as ""/)).toBeInTheDocument();
    expect(screen.getByRole('radio', { name: 'Loaded rows (2)' })).toBeChecked();
    await user.click(screen.getByRole('button', { name: 'Export' }));

    expect(await screen.findByText(/Exported 2 rows to/)).toBeInTheDocument();
    // The backend opens the save dialog; the UI only names the default file and format.
    expect(callsOf('export_pick_path')).toEqual([{ baseName: 'orders', format: 'csv' }]);
    expect(callsOf('export_rows')).toEqual([
      {
        kind: 'mysql',
        columns: [col('id'), col('name')],
        rows,
        opts: { format: 'csv', delimiter: ';', header: true, quote_all: true, bom: true, table_name: 'orders', formula_guard: false },
        path: 'C:\\out\\orders.csv',
      },
    ]);
    await user.click(screen.getByRole('button', { name: 'Open folder' }));
    expect(callsOf('reveal_export')).toEqual([{ path: 'C:\\out\\orders.csv' }]);
  });

  it('formula protection is opt-in', async () => {
    const user = userEvent.setup();
    mockBackend({ export_pick_path: () => 'C:\\out\\o.csv', export_rows: (a) => summary(2, a.path as string) });
    render(<ResultExportDialog request={request()} onClose={() => {}} />);
    await user.click(screen.getByRole('checkbox', { name: 'Protect against formula injection' }));
    await user.click(screen.getByRole('button', { name: 'Export' }));
    await screen.findByText(/Exported 2 rows/);
    expect(callsOf('export_rows')[0]!.opts).toMatchObject({ formula_guard: true });
  });

  it('SQL needs a table name; the dialog asks for the chosen format', async () => {
    const user = userEvent.setup();
    mockBackend({ export_pick_path: () => null });
    render(<ResultExportDialog request={request({ engine: 'postgres' })} onClose={() => {}} />);
    await user.selectOptions(screen.getByLabelText('Format'), 'sql');
    expect(screen.getByText('Quoted as one identifier for PostgreSQL.')).toBeInTheDocument();
    expect(screen.getByLabelText('Table name')).toHaveValue('orders');
    await user.clear(screen.getByLabelText('Table name'));
    expect(screen.getByRole('button', { name: 'Export' })).toBeDisabled();
    await user.type(screen.getByLabelText('Table name'), 'archive');
    await user.click(screen.getByRole('button', { name: 'Export' }));
    await waitFor(() => expect(callsOf('export_pick_path')).toEqual([{ baseName: 'orders', format: 'sql' }]));
    // The save dialog was cancelled: nothing is written.
    expect(callsOf('export_rows')).toEqual([]);
    expect(screen.getByRole('button', { name: 'Export' })).toBeEnabled();
  });

  it('all rows of the data view stream with progress and can be cancelled', async () => {
    const user = userEvent.setup();
    let reject: (e: unknown) => void = () => undefined;
    mockBackend({
      export_pick_path: () => 'C:\\out\\orders.xlsx',
      export_table: (a) => {
        const channel = a.channel as { onmessage(p: ExportProgress): void };
        channel.onmessage({ rows: 1500 });
        return new Promise((_, rej) => (reject = rej));
      },
      table_data_cancel: () => {
        reject(fail('cancelled').error);
        return true;
      },
    });
    render(<ResultExportDialog request={request()} onClose={() => {}} />);
    await user.selectOptions(screen.getByLabelText('Format'), 'xlsx');
    await user.click(screen.getByRole('radio', { name: 'All rows (read again)' }));
    await user.click(screen.getByRole('button', { name: 'Export' }));
    expect(await screen.findByText('Exporting… 1,500 rows written')).toBeInTheDocument();
    const call = callsOf('export_table')[0]!;
    expect(call).toMatchObject({
      id: ids.prod,
      obj,
      sort: [{ column: 'id', dir: 'desc' }],
      filter: null,
      opts: { format: 'xlsx' },
      path: 'C:\\out\\orders.xlsx',
    });
    expect(call.requestKey).toMatch(/^export:/);
    await user.click(screen.getByRole('button', { name: 'Cancel export' }));
    expect(callsOf('table_data_cancel')).toEqual([{ id: ids.prod, requestKey: call.requestKey }]);
    expect(await screen.findByText('The export was cancelled; no file was written.')).toBeInTheDocument();
  });

  it('all rows of a query result run on its tab, which is busy until the export ends', async () => {
    const user = userEvent.setup();
    let resolve: (v: unknown) => void = () => undefined;
    mockBackend({
      export_pick_path: () => 'C:\\out\\r.json',
      export_query: () => new Promise((res) => (resolve = res)),
    });
    render(
      <ResultExportDialog
        request={request({ all: { kind: 'query', tabId: 'tab-q', connectionId: ids.prod, planId: 'plan-1', index: 2 } })}
        onClose={() => {}}
      />,
    );
    await user.selectOptions(screen.getByLabelText('Format'), 'json');
    await user.click(screen.getByRole('radio', { name: 'All rows (read again)' }));
    expect(screen.getByText(/Runs the statement again on this tab's session/)).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Export' }));
    await waitFor(() => expect(callsOf('export_query')).toHaveLength(1));
    const call = callsOf('export_query')[0]!;
    expect(call).toMatchObject({ planId: 'plan-1', index: 2, opts: { format: 'json' }, path: 'C:\\out\\r.json' });
    expect(call.runId).toMatch(/^[0-9a-f-]{36}$/);
    // The tab shows as running under the export's run id.
    expect(exportRunOf('tab-q')).toBe(call.runId);
    await act(async () => resolve(summary(10_000, 'C:\\out\\r.json')));
    expect(await screen.findByText(/Exported 10,000 rows/)).toBeInTheDocument();
    expect(exportRunOf('tab-q')).toBeNull();
  });

  it("the tab's Stop cancels a query export; a failure updates the tab's transaction state", async () => {
    const user = userEvent.setup();
    let reject: (e: unknown) => void = () => undefined;
    mockBackend({
      export_pick_path: () => 'C:\\out\\r.csv',
      export_query: () => new Promise((_, rej) => (reject = rej)),
      query_cancel: () => {
        reject(fail('cancelled', { tx_state: 'aborted' }).error);
        return null;
      },
    });
    render(
      <ResultExportDialog
        request={request({ all: { kind: 'query', tabId: 'tab-q', connectionId: ids.prod, planId: 'plan-1', index: 0 } })}
        onClose={() => {}}
      />,
    );
    await user.click(screen.getByRole('radio', { name: 'All rows (read again)' }));
    await user.click(screen.getByRole('button', { name: 'Export' }));
    await waitFor(() => expect(exportRunOf('tab-q')).not.toBeNull());
    const runId = exportRunOf('tab-q');
    await act(() => stopQuery('tab-q'));
    expect(callsOf('query_cancel')).toEqual([{ runId }]);
    expect(await screen.findByText('The export was cancelled; no file was written.')).toBeInTheDocument();
    expect(exportRunOf('tab-q')).toBeNull();
    expect(useQueryRun.getState().get('tab-q').txState).toBe('aborted');
  });

  it('partly loaded binary values: a note before, a count after', async () => {
    const user = userEvent.setup();
    mockBackend({ export_pick_path: () => 'C:\\out\\b.csv', export_rows: (a) => summary(2, a.path as string, 3) });
    render(<ResultExportDialog request={request({ partialBinaries: 3 })} onClose={() => {}} />);
    expect(screen.getByTestId('rx-partial-note')).toHaveTextContent(
      '3 binary values are only partially loaded — choose All rows to export them fully.',
    );
    await user.click(screen.getByRole('radio', { name: 'All rows (read again)' }));
    expect(screen.queryByTestId('rx-partial-note')).toBeNull();
    await user.click(screen.getByRole('radio', { name: 'Loaded rows (2)' }));
    await user.click(screen.getByRole('button', { name: 'Export' }));
    expect(await screen.findByText(/3 binary values were only partially loaded and written as empty cells/)).toBeInTheDocument();
  });

  it('partly loaded binary values without All rows say how they are written', () => {
    mockBackend({});
    render(<ResultExportDialog request={request({ partialBinaries: 1, all: null })} onClose={() => {}} />);
    expect(screen.getByTestId('rx-partial-note')).toHaveTextContent(
      '1 binary value is only partially loaded — they are exported as empty cells.',
    );
  });

  it('counts partly loaded binary values, array items included', () => {
    const partial: Value = { t: 'Bytes', v: { len: 100, preview: [1, 2] } };
    const whole: Value = { t: 'Bytes', v: { len: 2, preview: [1, 2] } };
    expect(countPartialBinaries([[partial, whole, { t: 'Array', v: [partial, { t: 'Null' }] }], [{ t: 'Null' }]])).toBe(2);
    expect(countPartialBinaries([])).toBe(0);
  });

  it('without a way to read again only the loaded rows are offered, with the reason', () => {
    mockBackend({});
    render(
      <ResultExportDialog
        request={request({ all: null, allUnavailable: 'Only the result of a read statement can be read again.', loadedNote: 'Only the first 2 rows were fetched.' })}
        onClose={() => {}}
      />,
    );
    expect(screen.getByRole('radio', { name: 'All rows (read again)' })).toBeDisabled();
    expect(screen.getByText('Only the result of a read statement can be read again.')).toBeInTheDocument();
    expect(screen.getByText('Only the first 2 rows were fetched.')).toBeInTheDocument();
  });

  it('a failed export shows the error and keeps the form', async () => {
    const user = userEvent.setup();
    mockBackend({
      export_pick_path: () => 'C:\\locked\\x.md',
      export_rows: () => {
        throw fail('connection_lost', { message: 'Could not write x.md: PermissionDenied' });
      },
    });
    render(<ResultExportDialog request={request()} onClose={() => {}} />);
    await user.selectOptions(screen.getByLabelText('Format'), 'markdown');
    await user.click(screen.getByRole('button', { name: 'Export' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('Could not write x.md: PermissionDenied');
    expect(screen.getByRole('button', { name: 'Export' })).toBeEnabled();
  });
});
