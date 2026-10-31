import { act, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { DataImportPreview, ImportRequest, ImportTarget, Inspection, TargetColumn } from '../../bindings';
import { rerun } from '../../jobs/rerun';
import { useConnections } from '../../store/connections';
import { useDialogs } from '../../store/dialogs';
import { useJobs } from '../../store/jobs';
import { useLayout } from '../../store/layout';
import { appError, callsOf, config, fail, ids, mockBackend, resetStores, serverInfo } from '../../test/backend';
import { sqlCaps } from '../../test/fixtures/caps';
import { jobDetail, jobSummary } from '../../test/jobs';
import { ImportWizard } from './ImportWizard';
import './rerunImport';

vi.mock('@tauri-apps/api/event', () => ({ listen: vi.fn(async () => () => {}) }));

const table = { database: 'shop', schema: null, name: 'orders', kind: 'Table' as const };

const column = (name: string, over: Partial<TargetColumn> = {}): TargetColumn => ({
  name,
  db_type: 'int',
  logical: 'Integer',
  nullable: true,
  has_default: false,
  auto_increment: false,
  identity: null,
  generated: false,
  enum_values: [],
  max_chars: null,
  collation: null,
  ...over,
});

const target = (over: Partial<ImportTarget> = {}): ImportTarget => ({
  columns: [
    column('id', { nullable: false, auto_increment: true, has_default: true }),
    column('customer', { db_type: 'varchar(40)', logical: 'Text', nullable: false }),
    column('total', { db_type: 'decimal(10,2)', logical: 'Decimal' }),
  ],
  keys: [
    { name: 'PRIMARY', columns: ['id'], primary: true },
    { name: 'uq_customer', columns: ['customer'], primary: false },
  ],
  atomicity: 'yes',
  env: null,
  read_only: false,
  server: { kind: 'mysql', variant: 'MySql8', version: [8, 0, 36], version_string: '8.0.36', capabilities: sqlCaps('db') },
  ...over,
});

const inspection: Inspection = {
  columns: ['id', 'Customer', 'amount'],
  rows: [
    { number: 2, cells: [{ t: 'Text', v: '1' }, { t: 'Text', v: 'Ann' }, { t: 'Text', v: '9.50' }] },
    { number: 3, cells: [{ t: 'Text', v: '2' }, { t: 'Text', v: 'Bob' }, { t: 'Null' }] },
    { number: 4, cells: [{ t: 'Text', v: '3' }, { t: 'Text', v: 'Cy' }, { t: 'Text', v: '1.00' }] },
  ],
  sheets: [],
  delimiter: ',',
  total_bytes: 48,
};

const preview = (over: Partial<DataImportPreview> = {}): DataImportPreview => ({
  plan_id: 'plan-1',
  file: { name: 'orders.csv', size: 48, sha256: 'abcdef0123456789abcdef' },
  table,
  transactional: true,
  pre_step: null,
  insert_head: 'INSERT INTO `orders` (`id`, `customer`, `total`) VALUES ',
  insert_tail: ' ON DUPLICATE KEY UPDATE `customer` = VALUES(`customer`)',
  sample: "INSERT INTO `orders` (`id`, `customer`, `total`) VALUES (1, 'Ann', 9.50)",
  warnings: ['Rows matching any unique key are updated.'],
  confirm: { kind: 'none' },
  irreversible_name: null,
  expires_in_seconds: 600,
  ...over,
});

function backend(over: Record<string, (args: Record<string, unknown>) => unknown> = {}) {
  return mockBackend({
    import_pick_file: () => 'C:\\data\\orders.csv',
    import_inspect: () => inspection,
    import_target: () => target(),
    import_auto_map: () => [
      { target: 'id', source: 0 },
      { target: 'customer', source: 1 },
      { target: 'total', source: null },
    ],
    import_check_mapping: (args) => {
      const mode = args.mode as ImportRequest['mode'];
      if (mode.kind === 'upsert' && mode.key.length === 0) return { errors: [{ column: null, message: 'Choose a key.' }], warnings: [] };
      return { errors: [], warnings: [] };
    },
    import_dry_run: () => 'dry-1',
    import_prepare: () => preview(),
    import_start: () => 'job-9',
    job_list: () => [],
    job_detail: () => jobDetail({ id: 'dry-1' }),
    list_databases: () => [{ name: 'shop' }],
    list_objects: () => [{ name: 'orders', kind: 'Table', comment: null }],
    ...over,
  });
}

const user = () => userEvent.setup();

function open(props: { table?: typeof table; params?: ImportRequest } = { table }) {
  const onClose = vi.fn();
  render(<ImportWizard connectionId={ids.prod} table={props.table} params={props.params} onClose={onClose} />);
  return onClose;
}

const next = () => screen.getByRole('button', { name: 'Next' });

/** From an empty wizard to the Mapping step with the auto mapping in place. */
async function toMapping(u: ReturnType<typeof user>) {
  await u.click(screen.getByRole('button', { name: 'Choose file…' }));
  await screen.findByTestId('import-preview');
  await waitFor(() => expect(next()).toBeEnabled());
  await u.click(next());
  await screen.findByTestId('import-target-columns');
  await waitFor(() => expect(next()).toBeEnabled());
  await u.click(next());
  await waitFor(() => expect(screen.getByLabelText('Source for customer')).toHaveValue('1'));
}

beforeEach(() => {
  resetStores();
  useConnections.getState().setStored([], [config(ids.prod, 'shop-prod')]);
  useConnections.getState().setInfo(ids.prod, serverInfo('MySql8'));
});

describe('ImportWizard', () => {
  it('pick, preview, target, auto map, options, dry run, review and start', async () => {
    backend();
    const u = user();
    const onClose = open();
    expect(next()).toBeDisabled();

    await u.click(screen.getByRole('button', { name: 'Choose file…' }));
    const grid = await screen.findByTestId('import-preview');
    expect(within(grid).getAllByRole('row')).toHaveLength(4); // header + 3 rows
    expect(grid).toHaveTextContent('Customer');
    expect(grid).toHaveTextContent('Bob');
    expect(callsOf('import_inspect')).toEqual([
      { path: 'C:\\data\\orders.csv', source: { format: 'csv', delimiter: null, header: true, sheet: null } },
    ]);

    await u.click(next());
    const cols = await screen.findByTestId('import-target-columns');
    expect(cols).toHaveTextContent('varchar(40)');
    expect(cols).toHaveTextContent('auto increment');
    expect(callsOf('import_target')).toEqual([{ connectionId: ids.prod, table }]);

    await waitFor(() => expect(next()).toBeEnabled());
    await u.click(next());
    await waitFor(() => expect(screen.getByLabelText('Source for customer')).toHaveValue('1'));
    expect(callsOf('import_auto_map')).toEqual([{ source: ['id', 'Customer', 'amount'], targets: target().columns }]);
    // Change one mapping: the backend checks it again.
    await u.selectOptions(screen.getByLabelText('Source for total'), '2');
    await waitFor(() => expect(callsOf('import_check_mapping').at(-1)?.mapping).toEqual([
      { target: 'id', source: 0 },
      { target: 'customer', source: 1 },
      { target: 'total', source: 2 },
    ]));
    await waitFor(() => expect(next()).toBeEnabled());
    await u.click(next());

    // Options: Upsert needs a key; the primary key is the default.
    await u.click(screen.getByLabelText('Upsert'));
    expect(screen.getByLabelText('Upsert key')).toHaveValue('PRIMARY');
    await waitFor(() => expect(next()).toBeEnabled());
    await u.click(next());

    // Check: the dry run is a job; its progress comes from the jobs store.
    await u.click(screen.getByRole('button', { name: 'Validate file' }));
    expect(callsOf('import_dry_run')[0]).toMatchObject({
      request: { server_check: false, request: { mode: { kind: 'upsert', key: ['id'] }, path: 'C:\\data\\orders.csv' } },
    });
    const job = jobSummary({ id: 'dry-1', kind: 'import_dry_run', state: 'running', progress: { phase: 'Checking', done: 2, total: null, unit: 'rows' } });
    act(() => useJobs.getState().apply({ job, new_log: [] }));
    const status = await screen.findByTestId('import-dry-run');
    expect(status).toHaveTextContent('Checking · 2 rows');

    backend({
      job_detail: () =>
        jobDetail(
          { id: 'dry-1', state: 'done' },
          { log: [{ at: '2026-10-25T10:00:00.000Z', level: 'error', message: 'Row 3, column total: not a number' }] },
        ),
    });
    act(() =>
      useJobs.getState().apply({ job: { ...job, state: 'done', summary: '2 rows valid, 1 rows rejected (first errors in the log)' }, new_log: [] }),
    );
    expect(await within(status).findByText('Row 3, column total: not a number')).toBeInTheDocument();
    expect(status).toHaveTextContent('2 rows valid');

    // Review.
    await u.click(next());
    const review = await screen.findByTestId('import-review');
    expect(review).toHaveTextContent('orders.csv');
    expect(review).toHaveTextContent('abcdef012345');
    expect(review).toHaveTextContent('ON DUPLICATE KEY UPDATE');
    expect(review).toHaveTextContent('Rows matching any unique key are updated.');
    expect(callsOf('import_prepare')[0]?.request).toMatchObject({ mode: { kind: 'upsert', key: ['id'] } });

    await u.click(screen.getByRole('button', { name: 'Start import' }));
    expect(callsOf('import_start')).toEqual([{ planId: 'plan-1', confirm: { confirmed: true, name: null, irreversible_ack: false } }]);
    await waitFor(() => expect(onClose).toHaveBeenCalled());
    expect(useLayout.getState().rightVisible).toBe(true);
    expect(useLayout.getState().rightActivity).toBe('jobs');
  });

  it('Production needs the exact table name and the irreversible pre-step its acknowledgement', async () => {
    backend({
      import_target: () => target({ env: 'production', atomicity: 'no' }),
      import_prepare: () =>
        preview({
          transactional: false,
          pre_step: { sql: 'TRUNCATE TABLE `orders`', irreversible: true, warning: 'Existing rows are deleted even if the import fails later.' },
          confirm: { kind: 'type_name', name: 'orders' },
          irreversible_name: 'orders',
        }),
    });
    const u = user();
    open();
    await toMapping(u);
    await u.click(next());
    await u.click(screen.getByLabelText('Replace all'));
    await u.click(screen.getByLabelText('Truncate'));
    await waitFor(() => expect(next()).toBeEnabled());
    await u.click(next());
    await u.click(next());
    const review = await screen.findByTestId('import-review');
    expect(review).toHaveTextContent('TRUNCATE TABLE `orders`');
    expect(review).toHaveTextContent('Not transactional');

    const start = screen.getByRole('button', { name: 'Start import' });
    expect(start).toBeDisabled();
    await u.type(screen.getByLabelText(/Type orders to confirm/), 'Orders');
    expect(start).toBeDisabled();
    await u.clear(screen.getByLabelText(/Type orders to confirm/));
    await u.type(screen.getByLabelText(/Type orders to confirm/), 'orders');
    expect(start).toBeDisabled();
    await u.click(screen.getByLabelText(/I understand that existing rows are deleted/));
    expect(start).toBeEnabled();
    await u.click(start);
    expect(callsOf('import_start')).toEqual([{ planId: 'plan-1', confirm: { confirmed: true, name: 'orders', irreversible_ack: true } }]);
  });

  it('Staging asks for "I understand"', async () => {
    backend({ import_target: () => target({ env: 'staging' }), import_prepare: () => preview({ confirm: { kind: 'click' } }) });
    const u = user();
    open();
    await toMapping(u);
    await u.click(next());
    await u.click(next());
    await u.click(next());
    await screen.findByTestId('import-review');
    const start = screen.getByRole('button', { name: 'Start import' });
    expect(start).toBeDisabled();
    await u.click(screen.getByLabelText(/I understand/));
    await u.click(start);
    expect(callsOf('import_start')[0]?.confirm).toEqual({ confirmed: true, name: null, irreversible_ack: false });
  });

  it('PlanStale on start says so and returns to the review to prepare again', async () => {
    backend({
      import_start: () => {
        throw fail('plan_stale');
      },
    });
    const u = user();
    const onClose = open();
    await toMapping(u);
    await u.click(next());
    await u.click(next());
    await u.click(next());
    await screen.findByTestId('import-review');
    await u.click(screen.getByRole('button', { name: 'Start import' }));
    expect(await screen.findByText('The file or the table changed; prepare again.')).toBeInTheDocument();
    expect(onClose).not.toHaveBeenCalled();
    expect(screen.getByRole('button', { name: 'Start import' })).toBeDisabled();
    await u.click(screen.getByRole('button', { name: 'Prepare again' }));
    await waitFor(() => expect(callsOf('import_prepare')).toHaveLength(2));
    await waitFor(() => expect(screen.getByRole('button', { name: 'Start import' })).toBeEnabled());
  });

  it('mapping errors from the backend are shown inline and keep Next disabled', async () => {
    backend({
      import_check_mapping: () => ({ errors: [{ column: 'customer', message: 'NOT NULL without a default must be mapped.' }], warnings: [] }),
      import_auto_map: () => [
        { target: 'id', source: 0 },
        { target: 'customer', source: null },
        { target: 'total', source: null },
      ],
    });
    const u = user();
    open();
    await u.click(screen.getByRole('button', { name: 'Choose file…' }));
    await screen.findByTestId('import-preview');
    await u.click(next());
    await screen.findByTestId('import-target-columns');
    await waitFor(() => expect(next()).toBeEnabled());
    await u.click(next());
    expect(await screen.findByText('NOT NULL without a default must be mapped.')).toBeInTheDocument();
    expect(next()).toBeDisabled();
  });

  it('options reach the request with the wire names; batch fields clamp to the backend limits', async () => {
    backend();
    const u = user();
    open();
    await toMapping(u);
    await u.click(next());
    await u.selectOptions(screen.getByLabelText('Binary columns'), 'Base64');
    const rows = screen.getByLabelText('Rows per batch');
    await u.clear(rows);
    // An empty box while retyping does not snap back to the old value.
    expect(rows).toHaveValue(null);
    await u.type(rows, '150000');
    const kib = screen.getByLabelText('Batch size (KiB)');
    await u.clear(kib);
    await u.type(kib, '99999');
    await u.tab();
    expect(kib).toHaveValue(65536);
    await u.click(screen.getByLabelText('Skip and log the row'));
    await waitFor(() => expect(next()).toBeEnabled());
    await u.click(next());
    await u.click(next());
    await screen.findByTestId('import-review');
    await waitFor(() => expect(callsOf('import_prepare')).toHaveLength(1));
    expect(callsOf('import_prepare')[0]?.request).toMatchObject({
      convert: { binary: 'base64' },
      batch_rows: 100000,
      batch_bytes: 65536 * 1024,
      errors: { kind: 'skip', max_errors: 1000 },
    });
  });

  it('Start opens the Jobs panel on the started job', async () => {
    backend();
    const u = user();
    open();
    await toMapping(u);
    await u.click(next());
    await u.click(next());
    await u.click(next());
    await screen.findByTestId('import-review');
    await waitFor(() => expect(screen.getByRole('button', { name: 'Start import' })).toBeEnabled());
    await u.click(screen.getByRole('button', { name: 'Start import' }));
    await waitFor(() => expect(useJobs.getState().openId).toBe('job-9'));
  });

  it('a failed review is prepared again after the request changed', async () => {
    let n = 0;
    backend({
      import_prepare: () => {
        n++;
        if (n === 1) throw fail('plan_stale');
        return preview();
      },
    });
    const u = user();
    open();
    await toMapping(u);
    await u.click(next());
    await u.click(next());
    await u.click(next());
    expect(await screen.findByText('The file or the table changed; prepare again.')).toBeInTheDocument();
    await u.click(screen.getByRole('button', { name: /Options$/ }));
    await u.click(screen.getByLabelText('Trim spaces around text'));
    await u.click(screen.getByRole('button', { name: /Review$/ }));
    await waitFor(() => expect(screen.getByRole('button', { name: 'Start import' })).toBeEnabled());
    expect(screen.queryByText('The file or the table changed; prepare again.')).toBeNull();
    expect(callsOf('import_prepare')).toHaveLength(2);
  });

  it('a return to the Review with an unchanged request reuses the plan (the backend keeps a bounded number)', async () => {
    let n = 0;
    backend({ import_prepare: () => preview({ plan_id: `plan-${++n}` }) });
    const u = user();
    open();
    await toMapping(u);
    await u.click(next());
    await u.click(next());
    await u.click(next());
    await waitFor(() => expect(screen.getByRole('button', { name: 'Start import' })).toBeEnabled());
    expect(callsOf('import_prepare')).toHaveLength(1);

    // Changed and changed back: the same request, so the same plan.
    await u.click(screen.getByRole('button', { name: /Options$/ }));
    await u.click(screen.getByLabelText('Trim spaces around text'));
    await u.click(screen.getByLabelText('Trim spaces around text'));
    await u.click(screen.getByRole('button', { name: /Review$/ }));
    await waitFor(() => expect(screen.getByRole('button', { name: 'Start import' })).toBeEnabled());
    expect(callsOf('import_prepare')).toHaveLength(1);

    // A changed request prepares a new plan.
    await u.click(screen.getByRole('button', { name: /Options$/ }));
    await u.click(screen.getByLabelText('Trim spaces around text'));
    await u.click(screen.getByRole('button', { name: /Review$/ }));
    await waitFor(() => expect(callsOf('import_prepare')).toHaveLength(2));
    await waitFor(() => expect(screen.getByRole('button', { name: 'Start import' })).toBeEnabled());
    await u.click(screen.getByRole('button', { name: 'Start import' }));
    expect(callsOf('import_start')[0]?.planId).toBe('plan-2');
  });

  it('read-only connections cannot validate on the server', async () => {
    backend({ import_target: () => target({ read_only: true }) });
    const u = user();
    open();
    await toMapping(u);
    await u.click(next());
    await u.click(next());
    expect(screen.getByRole('button', { name: 'Validate on server (temporary table)' })).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Validate file' })).toBeEnabled();
  });
});

describe('Run again', () => {
  it('reopens the wizard pre-filled from the stored request', async () => {
    const request: ImportRequest = {
      connection_id: ids.prod,
      table,
      path: 'C:\\data\\orders.csv',
      source: { format: 'csv', delimiter: ';', header: true, sheet: null },
      mapping: [{ target: 'id', source: 0 }, { target: 'customer', source: 1 }, { target: 'total', source: 2 }],
      mode: { kind: 'replace', truncate: false },
      errors: { kind: 'skip', max_errors: 10 },
      convert: { empty: 'null', null_text: '\\N', trim: true, binary: 'hex' },
      batch_rows: 200,
      batch_bytes: 65536,
      override_identity: false,
    };
    expect(rerun({ kind: 'import', request: request as never })).toBe(true);
    expect(useDialogs.getState().request).toEqual({ kind: 'importData', connectionId: ids.prod, table, params: request });
    expect(rerun({ kind: 'import_dry_run', request: { request, server_check: true } as never })).toBe(true);
    expect(useDialogs.getState().request).toMatchObject({ kind: 'importData', params: request });

    backend();
    const u = user();
    open({ table, params: request });
    await screen.findByTestId('import-preview');
    expect(callsOf('import_inspect')[0]?.source).toEqual(request.source);
    await u.click(next());
    await screen.findByTestId('import-target-columns');
    await waitFor(() => expect(next()).toBeEnabled());
    await u.click(next());
    // The stored mapping is kept, not replaced by the auto mapping.
    expect(screen.getByLabelText('Source for total')).toHaveValue('2');
    expect(callsOf('import_auto_map')).toHaveLength(0);
  });

  it('after a restart the file must be chosen again; the same file keeps its options and mapping', async () => {
    const request: ImportRequest = {
      connection_id: ids.prod,
      table,
      path: 'C:\\data\\orders.csv',
      source: { format: 'csv', delimiter: ';', header: false, sheet: null },
      mapping: [{ target: 'id', source: 0 }, { target: 'customer', source: 1 }, { target: 'total', source: 2 }],
      mode: { kind: 'append' },
      errors: { kind: 'stop' },
      convert: { empty: 'null', null_text: null, trim: false, binary: 'hex' },
      batch_rows: 1000,
      batch_bytes: 1024 * 1024,
      override_identity: false,
    };
    // Admissions last one app session: the stored path is refused until it is picked again.
    let admitted = false;
    backend({
      import_pick_file: () => {
        admitted = true;
        return 'C:\\data\\orders.csv';
      },
      import_inspect: () => {
        if (!admitted) throw appError('config', 'Choose the file in the file dialog.', { field: 'path' });
        return inspection;
      },
    });
    const u = user();
    open({ table, params: request });
    expect(await screen.findByRole('alert')).toHaveTextContent('Choose the file again (files picked before a restart must be chosen again).');
    expect(next()).toBeDisabled();

    await u.click(screen.getByRole('button', { name: 'Choose file…' }));
    await screen.findByTestId('import-preview');
    expect(screen.queryByRole('alert')).toBeNull();
    expect(callsOf('import_inspect')).toHaveLength(2);
    expect(callsOf('import_inspect')[1]?.source).toEqual(request.source);
    expect(screen.getByLabelText('Delimiter')).toHaveValue(';');
    expect(screen.getByLabelText('First row holds the column names')).not.toBeChecked();
    await u.click(next());
    await screen.findByTestId('import-target-columns');
    await waitFor(() => expect(next()).toBeEnabled());
    await u.click(next());
    expect(screen.getByLabelText('Source for total')).toHaveValue('2');
    expect(callsOf('import_auto_map')).toHaveLength(0);
  });

  it('another file picked on Run again starts from its own defaults', async () => {
    const request = {
      connection_id: ids.prod,
      table,
      path: 'C:\\data\\old.csv',
      source: { format: 'csv', delimiter: ';', header: false, sheet: null },
      mapping: [{ target: 'id', source: 0 }],
      mode: { kind: 'append' },
      errors: { kind: 'stop' },
      convert: { empty: 'null', null_text: null, trim: false, binary: 'hex' },
      batch_rows: 1000,
      batch_bytes: 1024 * 1024,
      override_identity: false,
    } satisfies ImportRequest;
    backend();
    const u = user();
    open({ table, params: request });
    await screen.findByTestId('import-preview');
    await u.click(screen.getByRole('button', { name: 'Choose file…' }));
    await waitFor(() => expect(callsOf('import_inspect')).toHaveLength(2));
    expect(callsOf('import_inspect')[1]).toEqual({ path: 'C:\\data\\orders.csv', source: { format: 'csv', delimiter: null, header: true, sheet: null } });
  });
});
