import type { JobSummary } from '../../ui/src/bindings';
import { config, Failure, ids } from '../../ui/src/test/fixtures/data';
import { byId, expandRow, treeRow } from '../src/app';
import { createBackend } from '../src/backend';
import { IMPORT_FILE } from '../src/importFixtures';
import { jobDetail, jobEvent, jobProgress, jobSummary, logLine } from '../src/jobs';
import { emitEvent, installBackend } from '../src/shim';
import { expect, test } from '../src/test';

const JOB_EVENT = 'job://progress';

// The data import wizard (P5b) against the canned MySQL 8 `orders` on a Production connection:
// pick, preview, map, Upsert, the dry run as a job (progress from events), review, typed name, start.
// The fake backend answers with canned values; reading, mapping and the SQL are covered by the Rust tests.
test.describe('import wizard (browser, fake IPC backend)', () => {
  test('Import data… on orders (PROD): map, upsert, validate the file, type the name, start; the Jobs panel shows the import', async ({ page }) => {
    let dryRun: JobSummary | null = null;
    const importJob = jobSummary({ id: 'import-job', title: 'Import orders.csv into orders', progress: jobProgress({ phase: 'Rows', done: 0, total: null, unit: 'rows' }) });
    const backend = createBackend({
      // The dry run once it was started; the import once `import_start` was called.
      job_list: () => [
        ...(dryRun ? [dryRun] : []),
        ...(backend.calls.some((c) => c.cmd === 'import_start') ? [importJob] : []),
      ],
      job_detail: ({ id }) => {
        if (dryRun && id === dryRun.id) return jobDetail(dryRun, [logLine('Reading orders.csv'), logLine('Row 3, column total: empty, NULL used', 'warn')]);
        return jobDetail(importJob);
      },
    });
    await backend.handlers.create_connection!({ cfg: config(ids.prod, 'shop-prod', { env_tag: 'production', default_db: 'dnt_fixture' }).config });
    backend.calls.length = 0;
    await installBackend(page, backend.handlers);
    await page.goto('/');

    await expandRow(page, 'shop-prod');
    await expandRow(page, 'dnt_fixture');
    await expandRow(page, 'Tables');
    await treeRow(page, 'orders').click({ button: 'right' });
    await page.getByRole('menuitem', { name: 'Import data…' }).click();

    const wizard = byId(page, 'import-wizard');
    await expect(wizard).toBeVisible();
    await expect(wizard).toContainText('Import data into orders');
    const next = wizard.getByRole('button', { name: 'Next' });
    await expect(next).toBeDisabled();

    // Source: the picked file's preview, three records.
    await wizard.getByRole('button', { name: 'Choose file…' }).click();
    await expect(wizard.getByLabel('File', { exact: true })).toHaveValue(IMPORT_FILE);
    const preview = byId(page, 'import-preview');
    await expect(preview.locator('tbody tr')).toHaveCount(3);
    await expect(preview).toContainText('amount');
    await expect(preview).toContainText('42.00');
    await expect(next).toBeEnabled();
    await next.click();

    // Target: pre-filled from the tree.
    const columns = byId(page, 'import-target-columns');
    await expect(columns).toContainText('decimal(12,2)');
    await expect(columns).toContainText('auto increment');
    await expect(next).toBeEnabled();
    await next.click();

    // Mapping: auto-filled by name; `total` is mapped from `amount` by hand.
    await expect(wizard.getByLabel('Source for customer_id')).toHaveValue('1');
    const total = wizard.getByLabel('Source for total');
    await expect(total).toHaveValue('');
    await total.selectOption({ label: 'amount' });
    await expect
      .poll(() => backend.calls.filter((c) => c.cmd === 'import_check_mapping').at(-1)?.args.mapping)
      .toEqual([
        { target: 'id', source: 0 },
        { target: 'customer_id', source: 1 },
        { target: 'total', source: 2 },
        { target: 'created_at', source: null },
      ]);
    await expect(next).toBeEnabled();
    await next.click();

    // Options: Upsert on the primary key.
    await wizard.getByLabel('Upsert', { exact: true }).check();
    await expect(wizard.getByLabel('Upsert key')).toHaveValue('PRIMARY');
    await expect(next).toBeEnabled();
    await next.click();

    // Check: "Validate file" starts the dry run as a job; its progress comes from job events.
    await wizard.getByRole('button', { name: 'Validate file' }).click();
    await expect.poll(() => backend.calls.filter((c) => c.cmd === 'import_dry_run').length).toBe(1);
    const dry = backend.calls.find((c) => c.cmd === 'import_dry_run')!;
    expect(dry.args.request).toMatchObject({
      server_check: false,
      request: { path: IMPORT_FILE, table: { name: 'orders' }, mode: { kind: 'upsert', key: ['id'] } },
    });
    const status = byId(page, 'import-dry-run');
    await expect(status).toBeVisible();

    dryRun = jobSummary({ id: 'dry-1', kind: 'import_dry_run', title: 'Check orders.csv for orders', progress: jobProgress({ phase: 'Checking', done: 2, total: 3, unit: 'rows' }) });
    await emitEvent(page, JOB_EVENT, jobEvent(dryRun));
    await expect(status).toContainText('Checking · 2 / 3 rows');

    dryRun = { ...dryRun, state: 'done', finished_at: new Date().toISOString(), progress: jobProgress({ phase: 'Checking', done: 3, total: 3, unit: 'rows' }), summary: '3 rows valid' };
    await emitEvent(page, JOB_EVENT, jobEvent(dryRun));
    await expect(status).toContainText('3 rows valid');
    await expect(status).toContainText('Row 3, column total: empty, NULL used');
    await next.click();

    // Review: the INSERT template of the upsert; Production asks for the exact table name.
    const review = byId(page, 'import-review');
    await expect(review).toContainText('INSERT INTO `dnt_fixture`.`orders` (`id`, `customer_id`, `total`) VALUES');
    await expect(review).toContainText('ON DUPLICATE KEY UPDATE `customer_id` = VALUES(`customer_id`)');
    const startButton = wizard.getByRole('button', { name: 'Start import' });
    const confirmName = review.getByRole('textbox', { name: 'Type orders to confirm' });
    await expect(startButton).toBeDisabled();
    await confirmName.fill('order');
    await expect(startButton).toBeDisabled();
    await confirmName.fill('orders');
    await expect(startButton).toBeEnabled();
    await startButton.click();

    // Started: the wizard closes and the Jobs panel shows the import.
    await expect(wizard).toBeHidden();
    const start = backend.calls.filter((c) => c.cmd === 'import_start');
    expect(start).toHaveLength(1);
    const prepared = backend.calls.filter((c) => c.cmd === 'import_prepare');
    expect(prepared).toHaveLength(1);
    expect(start[0]!.args.planId).toMatch(/^import-plan-/);
    expect(start[0]!.args.confirm).toEqual({ confirmed: true, name: 'orders', irreversible_ack: false });
    await expect(byId(page, 'jobs-view')).toBeVisible();
    await expect(page.getByTestId('job-row').filter({ hasText: 'Import orders.csv into orders' })).toBeVisible();
  });

  test('Replace all with Truncate (MySQL): the dry run is listed as a job; the irreversible TRUNCATE needs its acknowledgement', async ({ page }) => {
    // The fake's own job handlers: the dry run it started is what `job_list` answers.
    const backend = createBackend();
    await backend.handlers.create_connection!({ cfg: config(ids.analytics, 'shop-dev', { default_db: 'dnt_fixture' }).config });
    backend.calls.length = 0;
    await installBackend(page, backend.handlers);
    await page.goto('/');

    await expandRow(page, 'shop-dev');
    await expandRow(page, 'dnt_fixture');
    await expandRow(page, 'Tables');
    await treeRow(page, 'orders').click({ button: 'right' });
    await page.getByRole('menuitem', { name: 'Import data…' }).click();

    const wizard = byId(page, 'import-wizard');
    const next = wizard.getByRole('button', { name: 'Next' });
    await wizard.getByRole('button', { name: 'Choose file…' }).click();
    await expect(byId(page, 'import-preview').locator('tbody tr')).toHaveCount(3);
    await next.click();
    await expect(byId(page, 'import-target-columns')).toContainText('decimal(12,2)');
    await next.click();
    await expect(wizard.getByLabel('Source for customer_id')).toHaveValue('1');
    await expect(next).toBeEnabled();
    await next.click();

    // Options: Replace all, rows removed by TRUNCATE.
    await wizard.getByLabel('Replace all').check();
    await wizard.getByLabel('Truncate').check();
    await expect(next).toBeEnabled();
    await next.click();

    // Check: the job manager lists the dry run it was given.
    await wizard.getByRole('button', { name: 'Validate file' }).click();
    const status = byId(page, 'import-dry-run');
    await expect(status).toContainText('File check · Running');
    const listed = (await backend.handlers.job_list!({})) as JobSummary[];
    expect(listed.map((j) => [j.id, j.kind])).toEqual([['dry-1', 'import_dry_run']]);
    await next.click();

    // Review: TRUNCATE commits at once; Start waits for the acknowledgement.
    const review = byId(page, 'import-review');
    await expect(review).toContainText('TRUNCATE `dnt_fixture`.`orders`');
    await expect(review).toContainText('Existing rows are deleted even if the import fails later.');
    const startButton = wizard.getByRole('button', { name: 'Start import' });
    await expect(startButton).toBeDisabled();
    await review.getByLabel('I understand that existing rows are deleted even if the import fails later').check();
    await expect(startButton).toBeEnabled();
    await startButton.click();

    await expect(wizard).toBeHidden();
    const start = backend.calls.filter((c) => c.cmd === 'import_start');
    expect(start).toHaveLength(1);
    expect(start[0]!.args.confirm).toEqual({ confirmed: true, name: null, irreversible_ack: true });
  });

  test('the fake refuses an irreversible pre-step without its acknowledgement, as the backend does', async () => {
    const backend = createBackend();
    await backend.handlers.create_connection!({ cfg: config(ids.analytics, 'shop-dev', { default_db: 'dnt_fixture' }).config });
    const request = {
      connection_id: ids.analytics,
      table: { database: 'dnt_fixture', schema: null, name: 'orders', kind: 'Table' },
      path: IMPORT_FILE,
      source: { format: 'csv', delimiter: null, header: true, sheet: null },
      mapping: [{ target: 'id', source: 0 }],
      mode: { kind: 'replace', truncate: true },
      errors: { kind: 'stop' },
      convert: { empty: 'null', null_text: null, trim: false, binary: 'hex' },
      batch_rows: 1000,
      batch_bytes: 1048576,
      override_identity: false,
    };
    const startWith = async (ack: boolean) => {
      const { plan_id } = (await backend.handlers.import_prepare!({ request })) as { plan_id: string };
      try {
        return await backend.handlers.import_start!({ planId: plan_id, confirm: { confirmed: true, name: null, irreversible_ack: ack } });
      } catch (e) {
        return (e as Failure).error.kind;
      }
    };
    expect(await startWith(false)).toBe('confirmation_required');
    expect(await startWith(true)).toMatch(/^import-/);
  });
});
