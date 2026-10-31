import type { JobSummary } from '../../ui/src/bindings';
import { config, ids } from '../../ui/src/test/fixtures/data';
import { byId, expandRow, treeRow } from '../src/app';
import { createBackend } from '../src/backend';
import { jobEvent } from '../src/jobs';
import { emitEvent, installBackend } from '../src/shim';
import { expect, test } from '../src/test';

const JOB_EVENT = 'job://progress';

// The data seeder against the canned MySQL 8 `orders`: the tree entry, the generators per column,
// the live preview, the review and its confirmation, and the job in the Jobs panel. The fake
// backend answers with canned values; generating, the SQL and the guard are covered by the Rust tests.
test.describe('data seeder (browser, fake IPC backend)', () => {
  test('Generate data… on orders: change a generator, the preview follows, review, generate; the job shows in Jobs', async ({ page }) => {
    const backend = createBackend();
    await backend.handlers.create_connection!({ cfg: config(ids.analytics, 'shop-dev', { default_db: 'dnt_fixture' }).config });
    backend.calls.length = 0;
    await installBackend(page, backend.handlers);
    await page.goto('/');

    await expandRow(page, 'shop-dev');
    await expandRow(page, 'dnt_fixture');
    await expandRow(page, 'Tables');
    await treeRow(page, 'orders').click({ button: 'right' });
    await page.getByRole('menuitem', { name: 'Generate data…' }).click();

    const dialog = byId(page, 'seed-dialog');
    await expect(dialog).toBeVisible();
    await expect(dialog.getByRole('heading', { name: 'Generate data — dnt_fixture.orders' })).toBeVisible();
    await expect(dialog.getByLabel('Generator for email')).toHaveValue('fake');
    await expect(dialog.getByLabel('Generator for total_with_tax')).toBeDisabled();

    const grid = byId(page, 'seed-preview');
    await expect(grid.locator('tbody tr')).toHaveCount(10);
    await expect(grid).toContainText('shipped');

    // A constant status: the preview is asked again with it.
    await dialog.getByLabel('Generator for status').selectOption('constant');
    await dialog.getByLabel('Value for status').fill('new');
    await expect(grid).not.toContainText('shipped');
    await expect(grid).toContainText('new');
    const last = backend.calls.filter((c) => c.cmd === 'seed_preview').at(-1)!;
    expect((last.args.request as { columns: { name: string; generator: unknown }[] }).columns.find((c) => c.name === 'status')?.generator).toEqual({
      kind: 'constant',
      value: 'new',
    });

    const reviewButton = dialog.getByRole('button', { name: 'Review…' });
    await expect(reviewButton).toBeEnabled();
    await reviewButton.click();
    const review = byId(page, 'seed-review');
    await expect(review).toContainText('Insert 1,000 rows into dnt_fixture.orders in 1 transaction (1 batch).');
    await expect(review).toContainText('INSERT INTO `dnt_fixture`.`orders`');
    await dialog.getByRole('button', { name: 'Generate' }).click();

    await expect(dialog).toBeHidden();
    const start = backend.calls.filter((c) => c.cmd === 'seed_start');
    expect(start).toHaveLength(1);
    expect(start[0]!.args.confirm).toEqual({ confirmed: true, name: null, irreversible_ack: false });
    // The Jobs panel opens on the job just started; its progress and outcome come from job events.
    const jobs = byId(page, 'jobs-view');
    await expect(jobs).toBeVisible();
    await expect(jobs).toContainText('Generate data for orders');
    await expect(jobs).toContainText('Generating · 0 / 1,000 rows');

    const listed = (await backend.handlers.job_list!({})) as JobSummary[];
    const job = listed.find((j) => j.kind === 'seed')!;
    await emitEvent(
      page,
      JOB_EVENT,
      jobEvent({ ...job, state: 'done', finished_at: new Date().toISOString(), summary: 'Inserted 1,000 rows into orders in 1 batch' }),
    );
    await expect(jobs).toContainText('Inserted 1,000 rows into orders in 1 batch');
    await expect(jobs).toContainText('Done');
  });

  test('Production with Delete first: Generate stays disabled until the table name is typed', async ({ page }) => {
    const backend = createBackend();
    await backend.handlers.create_connection!({ cfg: config(ids.prod, 'shop-prod', { env_tag: 'production', default_db: 'dnt_fixture' }).config });
    backend.calls.length = 0;
    await installBackend(page, backend.handlers);
    await page.goto('/');

    await expandRow(page, 'shop-prod');
    await expandRow(page, 'dnt_fixture');
    await expandRow(page, 'Tables');
    await treeRow(page, 'orders').click({ button: 'right' });
    await page.getByRole('menuitem', { name: 'Generate data…' }).click();

    const dialog = byId(page, 'seed-dialog');
    await expect(byId(page, 'seed-preview').locator('tbody tr')).toHaveCount(10);
    await dialog.getByLabel('Existing rows').selectOption('delete');
    const reviewButton = dialog.getByRole('button', { name: 'Review…' });
    await expect(reviewButton).toBeEnabled();
    await reviewButton.click();

    const review = byId(page, 'seed-review');
    await expect(review).toContainText('DELETE FROM `dnt_fixture`.`orders`');
    const generate = dialog.getByRole('button', { name: 'Generate' });
    const confirmName = review.getByRole('textbox', { name: 'Type orders to confirm' });
    await expect(generate).toBeDisabled();
    await confirmName.fill('order');
    await expect(generate).toBeDisabled();
    await confirmName.fill('orders');
    await expect(generate).toBeEnabled();
    await generate.click();

    await expect(dialog).toBeHidden();
    const start = backend.calls.filter((c) => c.cmd === 'seed_start');
    expect(start[0]!.args.confirm).toEqual({ confirmed: true, name: 'orders', irreversible_ack: false });
    const prepared = backend.calls.filter((c) => c.cmd === 'seed_prepare');
    expect(prepared[0]!.args.request).toMatchObject({ existing: 'delete', table: { name: 'orders' } });
  });

  test('Read-only connection: Generate data… is disabled', async ({ page }) => {
    const backend = createBackend();
    await backend.handlers.create_connection!({ cfg: config(ids.analytics, 'shop-ro', { read_only: true, default_db: 'dnt_fixture' }).config });
    await installBackend(page, backend.handlers);
    await page.goto('/');

    await expandRow(page, 'shop-ro');
    await expandRow(page, 'dnt_fixture');
    await expandRow(page, 'Tables');
    await treeRow(page, 'orders').click({ button: 'right' });
    await expect(page.getByRole('menuitem', { name: 'Generate data…' })).toBeDisabled();
  });
});
