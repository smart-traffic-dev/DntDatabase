import { config, ids } from '../../ui/src/test/fixtures/data';
import { byId, expandRow, treeRow } from '../src/app';
import { createBackend } from '../src/backend';
import { DUMP_FILE, MYSQL_NOT_TRANSACTIONAL, PSQL_FILE, PSQL_FILE_MESSAGE, RESTORE_FILE } from '../src/backupFixtures';
import { jobDetail, jobProgress, jobSummary } from '../src/jobs';
import { installBackend } from '../src/shim';
import { expect, test } from '../src/test';

// The dump and restore dialogs (P5c) against the canned MySQL 8 `dnt_fixture`, opened from the
// database's context menu. The fake backend answers with canned values; dumping, detecting and
// running files are covered by the Rust tests.
test.describe('dump and restore dialogs (browser, fake IPC backend)', () => {
  test('Dump… on dnt_fixture: objects and skipped ones listed, choose the file, start; the Jobs panel shows the dump', async ({ page }) => {
    const dumpJob = jobSummary({ id: 'dump-job', kind: 'dump', title: 'Dump dnt_fixture to dnt_fixture.sql', progress: jobProgress({ phase: 'orders', done: 0, total: 2, unit: 'none' }) });
    const backend = createBackend({
      job_list: () => (backend.calls.some((c) => c.cmd === 'dump_start') ? [dumpJob] : []),
      job_detail: () => jobDetail(dumpJob),
    });
    await backend.handlers.create_connection!({ cfg: config(ids.analytics, 'shop-dev', { default_db: 'dnt_fixture' }).config });
    backend.calls.length = 0;
    await installBackend(page, backend.handlers);
    await page.goto('/');

    await expandRow(page, 'shop-dev');
    await treeRow(page, 'dnt_fixture').click({ button: 'right' });
    await page.getByRole('menuitem', { name: 'Dump…' }).click();

    const dialog = byId(page, 'dump-dialog');
    await expect(dialog).toBeVisible();
    await expect(dialog).toContainText('Dump dnt_fixture');
    await expect.poll(() => backend.calls.find((c) => c.cmd === 'dump_objects')?.args).toEqual({ connectionId: ids.analytics, database: 'dnt_fixture', schemas: [] });

    // Objects: the table (with its data toggle) and the view, all selected; the skipped procedure with its reason.
    const objects = byId(page, 'dump-objects');
    await expect(objects.getByRole('group', { name: 'Tables' })).toContainText('orders');
    await expect(objects.getByRole('group', { name: 'Views' })).toContainText('order_totals');
    await expect(objects.getByLabel('Data of orders')).toBeChecked();
    const skipped = byId(page, 'dump-skipped');
    await expect(skipped).toContainText('Not in the built-in dump (1)');
    await skipped.locator('summary').click();
    await expect(skipped).toContainText('archive_orders (Procedure): routines need an external dump tool');

    // Start needs an output file.
    const startButton = dialog.getByRole('button', { name: 'Start dump' });
    await expect(startButton).toBeDisabled();
    await dialog.getByRole('button', { name: 'Browse…' }).click();
    await expect(dialog).toContainText(DUMP_FILE);
    expect(backend.calls.find((c) => c.cmd === 'dump_pick_path')?.args).toEqual({ baseName: 'dnt_fixture', compress: false, format: null });

    // Only the table: the view is left out.
    await objects.getByRole('group', { name: 'Views' }).getByRole('checkbox').uncheck();
    await expect(startButton).toBeEnabled();
    await startButton.click();

    // Started: the dialog closes and the Jobs panel shows the dump.
    await expect(dialog).toBeHidden();
    const start = backend.calls.filter((c) => c.cmd === 'dump_start');
    expect(start).toHaveLength(1);
    expect(start[0]!.args.request).toEqual({
      connection_id: ids.analytics,
      database: 'dnt_fixture',
      schemas: [],
      objects: [{ schema: null, name: 'orders', kind: 'Table', data: true }],
      include_drop: false,
      compress: false,
      path: DUMP_FILE,
    });
    await expect(byId(page, 'jobs-view')).toBeVisible();
    await expect(page.getByTestId('job-row').filter({ hasText: 'Dump dnt_fixture to dnt_fixture.sql' })).toBeVisible();
  });

  test('Restore… on dnt_fixture (PROD): analyze, built-in runner with 12 statements, type the database name, start', async ({ page }) => {
    const restoreJob = jobSummary({ id: 'restore-job', kind: 'restore', title: 'Restore dnt_fixture.sql into dnt_fixture' });
    const backend = createBackend({
      job_list: () => (backend.calls.some((c) => c.cmd === 'restore_start') ? [restoreJob] : []),
      job_detail: () => jobDetail(restoreJob),
    });
    await backend.handlers.create_connection!({ cfg: config(ids.prod, 'shop-prod', { env_tag: 'production', default_db: 'dnt_fixture' }).config });
    backend.calls.length = 0;
    await installBackend(page, backend.handlers);
    await page.goto('/');

    await expandRow(page, 'shop-prod');
    await treeRow(page, 'dnt_fixture').click({ button: 'right' });
    await page.getByRole('menuitem', { name: 'Restore…' }).click();

    const dialog = byId(page, 'restore-dialog');
    await expect(dialog).toBeVisible();
    await expect(dialog.getByLabel('Database')).toHaveValue('dnt_fixture');
    const analyzeButton = dialog.getByRole('button', { name: 'Analyze' });
    await expect(analyzeButton).toBeDisabled();
    await dialog.getByRole('button', { name: 'Browse…' }).click();
    await expect(dialog).toContainText(RESTORE_FILE);
    await analyzeButton.click();

    // The preview: the built-in runner, 12 statements; a MySQL restore is never in one transaction.
    const preview = byId(page, 'restore-preview');
    await expect(byId(page, 'restore-path')).toHaveText('Built-in runner');
    await expect(preview.getByRole('table', { name: 'Statements' }).getByRole('row', { name: /Statements/ })).toContainText('12');
    await expect(preview).toContainText('Not in one transaction.');
    await expect(preview).toContainText(MYSQL_NOT_TRANSACTIONAL);
    const previewed = backend.calls.filter((c) => c.cmd === 'restore_preview');
    expect(previewed).toHaveLength(1);
    expect(previewed[0]!.args.request).toEqual({
      connection_id: ids.prod,
      database: 'dnt_fixture',
      path: RESTORE_FILE,
      single_transaction: true,
      clean: false,
      no_owner: false,
    });

    // Production: the exact database name (case-sensitive) enables Start.
    const startButton = dialog.getByRole('button', { name: 'Start restore' });
    const confirmName = preview.getByRole('textbox', { name: 'Type dnt_fixture to confirm' });
    await expect(startButton).toBeDisabled();
    await confirmName.fill('DNT_FIXTURE');
    await expect(startButton).toBeDisabled();
    await confirmName.fill('dnt_fixture');
    await expect(startButton).toBeEnabled();
    await startButton.click();

    // Started: the dialog closes and the Jobs panel shows the restore.
    await expect(dialog).toBeHidden();
    const start = backend.calls.filter((c) => c.cmd === 'restore_start');
    expect(start).toHaveLength(1);
    expect(start[0]!.args).toEqual({ planId: expect.stringMatching(/^restore-plan-/), confirmed: true, name: 'dnt_fixture' });
    await expect(byId(page, 'jobs-view')).toBeVisible();
    await expect(page.getByTestId('job-row').filter({ hasText: 'Restore dnt_fixture.sql into dnt_fixture' })).toBeVisible();
  });

  test('a file that needs psql (not installed): the refusal is shown and there is no Start restore', async ({ page }) => {
    const backend = createBackend({ restore_pick_file: () => PSQL_FILE });
    await backend.handlers.create_connection!({ cfg: config(ids.analytics, 'shop-dev', { default_db: 'dnt_fixture' }).config });
    backend.calls.length = 0;
    await installBackend(page, backend.handlers);
    await page.goto('/');

    await expandRow(page, 'shop-dev');
    await treeRow(page, 'dnt_fixture').click({ button: 'right' });
    await page.getByRole('menuitem', { name: 'Restore…' }).click();

    const dialog = byId(page, 'restore-dialog');
    await expect(dialog).toBeVisible();
    await expect(dialog.getByRole('button', { name: 'Start restore' })).toBeVisible();
    await dialog.getByRole('button', { name: 'Browse…' }).click();
    await expect(dialog).toContainText(PSQL_FILE);
    await dialog.getByRole('button', { name: 'Analyze' }).click();

    // The reason, without the server version; nothing can be started from it.
    await expect(dialog.getByRole('alert')).toHaveText(PSQL_FILE_MESSAGE);
    await expect(byId(page, 'restore-preview')).toHaveCount(0);
    await expect(dialog.getByRole('button', { name: 'Start restore' })).toHaveCount(0);
    expect(backend.calls.filter((c) => c.cmd === 'restore_start')).toHaveLength(0);
  });
});
