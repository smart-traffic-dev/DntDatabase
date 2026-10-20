import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { save } from '@tauri-apps/plugin-dialog';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { ColumnMeta, ExportProgress, ObjectRef, Value } from '../bindings';
import { callsOf, fail, ids, mockBackend, resetStores } from '../test/backend';
import { ResultExportDialog } from './ResultExportDialog';
import type { ExportRequest } from './resultExport';

vi.mock('@tauri-apps/plugin-dialog', () => ({ save: vi.fn(), open: vi.fn() }));

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
    all: { kind: 'table', connectionId: ids.prod, obj, sort: [{ column: 'id', dir: 'desc' }], filter: null },
    allUnavailable: null,
    ...over,
  };
}

const summary = (n: number, path: string) => ({ rows: n, path, cut_cells: 0, elapsed_ms: 3 });

beforeEach(() => {
  resetStores();
  vi.mocked(save).mockReset();
});

describe('ResultExportDialog', () => {
  it('exports the loaded rows as CSV with the chosen options, then offers Open folder', async () => {
    const user = userEvent.setup();
    vi.mocked(save).mockResolvedValue('C:\\out\\orders.csv');
    mockBackend({ export_rows: (a) => summary(2, a.path as string), reveal_export: () => null });
    render(<ResultExportDialog request={request()} onClose={() => {}} />);
    expect(screen.getByLabelText('Format')).toHaveValue('csv');
    await user.selectOptions(screen.getByLabelText('Delimiter'), 'Semicolon (;)');
    await user.click(screen.getByRole('checkbox', { name: 'UTF-8 BOM (for Excel)' }));
    await user.click(screen.getByRole('checkbox', { name: 'Quote all fields' }));
    expect(screen.getByRole('radio', { name: 'Loaded rows (2)' })).toBeChecked();
    await user.click(screen.getByRole('button', { name: 'Export' }));

    expect(await screen.findByText(/Exported 2 rows to/)).toBeInTheDocument();
    expect(vi.mocked(save).mock.calls[0]![0]).toEqual({
      defaultPath: 'orders.csv',
      filters: [{ name: 'CSV', extensions: ['csv'] }],
    });
    expect(callsOf('export_rows')).toEqual([
      {
        kind: 'mysql',
        columns: [col('id'), col('name')],
        rows,
        opts: { format: 'csv', delimiter: ';', header: true, quote_all: true, bom: true, table_name: 'orders' },
        path: 'C:\\out\\orders.csv',
      },
    ]);
    await user.click(screen.getByRole('button', { name: 'Open folder' }));
    expect(callsOf('reveal_export')).toEqual([{ path: 'C:\\out\\orders.csv' }]);
  });

  it('SQL needs a table name; the file filter follows the format', async () => {
    const user = userEvent.setup();
    vi.mocked(save).mockResolvedValue(null);
    mockBackend({});
    render(<ResultExportDialog request={request({ engine: 'postgres' })} onClose={() => {}} />);
    await user.selectOptions(screen.getByLabelText('Format'), 'sql');
    expect(screen.getByText('Quoted as one identifier for PostgreSQL.')).toBeInTheDocument();
    expect(screen.getByLabelText('Table name')).toHaveValue('orders');
    await user.clear(screen.getByLabelText('Table name'));
    expect(screen.getByRole('button', { name: 'Export' })).toBeDisabled();
    await user.type(screen.getByLabelText('Table name'), 'archive');
    await user.click(screen.getByRole('button', { name: 'Export' }));
    await waitFor(() => expect(save).toHaveBeenCalled());
    expect(vi.mocked(save).mock.calls[0]![0]).toEqual({ defaultPath: 'orders.sql', filters: [{ name: 'SQL', extensions: ['sql'] }] });
    // The save dialog was cancelled: nothing is written.
    expect(callsOf('export_rows')).toEqual([]);
    expect(screen.getByRole('button', { name: 'Export' })).toBeEnabled();
  });

  it('all rows of the data view stream with progress and can be cancelled', async () => {
    const user = userEvent.setup();
    vi.mocked(save).mockResolvedValue('C:\\out\\orders.xlsx');
    let reject: (e: unknown) => void = () => undefined;
    mockBackend({
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

  it('all rows of a query result re-read its statement', async () => {
    const user = userEvent.setup();
    vi.mocked(save).mockResolvedValue('C:\\out\\r.json');
    mockBackend({ export_query: (a) => summary(10_000, a.path as string) });
    render(
      <ResultExportDialog
        request={request({ all: { kind: 'query', connectionId: ids.prod, planId: 'plan-1', index: 2 } })}
        onClose={() => {}}
      />,
    );
    await user.selectOptions(screen.getByLabelText('Format'), 'json');
    await user.click(screen.getByRole('radio', { name: 'All rows (read again)' }));
    await user.click(screen.getByRole('button', { name: 'Export' }));
    expect(await screen.findByText(/Exported 10,000 rows/)).toBeInTheDocument();
    expect(callsOf('export_query')[0]).toMatchObject({ planId: 'plan-1', index: 2, opts: { format: 'json' }, path: 'C:\\out\\r.json' });
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
    vi.mocked(save).mockResolvedValue('C:\\locked\\x.md');
    mockBackend({
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
