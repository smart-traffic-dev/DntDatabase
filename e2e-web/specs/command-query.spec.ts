import type { Page } from '@playwright/test';
import { config, ids } from '../../ui/src/test/fixtures/data';
import { byId, expandRow, pickOption, treeRow } from '../src/app';
import { createBackend } from '../src/backend';
import { installBackend } from '../src/shim';
import { expect, test } from '../src/test';

// A Redis query tab against the fake Command server: the commands run through the same plan →
// confirm → run flow as SQL, results land in the grid, and History lists what ran.

const redis = (id: string, name: string, env: 'production' | null) =>
  config(id, name, {
    kind: 'redis',
    port: 6379,
    username: '',
    env_tag: env,
    options: { version: 1, connect_timeout_s: 10, ssh_timeout_s: 10, charset: null, application_name: null, query_timeout_s: 0, driver: { kind: 'redis', db: 0 } },
  }).config;

const mongo = (id: string, name: string) =>
  config(id, name, {
    kind: 'mongo',
    port: 27017,
    username: '',
    default_db: 'shop',
    options: {
      version: 1,
      connect_timeout_s: 10,
      ssh_timeout_s: 10,
      charset: null,
      application_name: null,
      query_timeout_s: 0,
      driver: { kind: 'mongo', srv: false, auth_source: null, replica_set: null, direct_connection: false, extra_hosts: [] },
    },
  }).config;

async function setup(page: Page) {
  const backend = createBackend();
  await backend.handlers.create_connection!({ cfg: mongo(ids.legacy, 'docs') });
  await backend.handlers.create_connection!({ cfg: redis(ids.analytics, 'cache', null) });
  await backend.handlers.create_connection!({ cfg: redis(ids.prod, 'cache-prod', 'production') });
  await installBackend(page, backend.handlers);
  await page.goto('/');
  return backend;
}

/** A new query tab bound to the connection `name`. */
async function newTab(page: Page, name: string) {
  await byId(page, 'new-query').click();
  await byId(page, 'query-context').click();
  await pickOption(page, 'query-connection', name);
  await page.keyboard.press('Escape');
}

const editor = (page: Page) => page.getByTestId('query-editor').locator('.monaco-editor .view-lines');

/** Types `text` on a new line of the focused editor and runs the statement at the cursor. */
async function runLine(page: Page, text: string) {
  await editor(page).click();
  await page.keyboard.press('Control+End');
  if ((await editor(page).innerText()).trim() !== '') await page.keyboard.press('Enter');
  await page.keyboard.type(text);
  await page.keyboard.press('Control+Enter');
}

test.describe('query tab of a Redis connection (browser, fake IPC backend)', () => {
  test('SET then GET shows the value; History lists both commands', async ({ page }) => {
    const backend = await setup(page);
    await newTab(page, 'cache');
    // The editor speaks Redis: the first word of a line is the command keyword.
    await expect(page.getByTestId('query-editor').locator('[data-mode-id="redis"]')).toHaveCount(1);

    await runLine(page, 'SET a 1');
    await expect(page.getByTestId('messages')).toContainText('1 row affected');
    await runLine(page, 'GET a');
    const grid = byId(page, 'result-grid');
    await expect(grid).toBeVisible();
    // Glide draws on a canvas: the record view (Shift+Space on the first row) shows the row as text.
    // The first data column starts after the 32 px row marker; rows sit under a 36 px header.
    const box = (await grid.locator('canvas').first().boundingBox())!;
    await page.mouse.click(box.x + 60, box.y + 48);
    await page.keyboard.press('Shift+Space');
    const record = byId(page, 'record-view');
    await expect(record.getByRole('region', { name: 'value' }).locator('.gt-field-value')).toHaveText('1');

    // Statement ranges came from the backend (one per line), never from the SQL splitter.
    await expect
      .poll(() => backend.calls.filter((c) => c.cmd === 'statement_ranges').at(-1)?.args)
      // Monaco's line ending follows the platform.
      .toMatchObject({ text: expect.stringMatching(/^SET a 1\r?\nGET a$/), dialect: { engine: 'postgres' }, connId: ids.analytics });
    expect(backend.calls.filter((c) => c.cmd.startsWith('completion_'))).toEqual([]);

    await page.getByRole('navigation', { name: 'Right sidebar' }).getByRole('button', { name: 'History' }).click();
    const rows = page.getByTestId('history-row');
    await expect(rows.filter({ hasText: 'SET a 1' })).toHaveCount(1);
    await expect(rows.filter({ hasText: 'GET a' })).toHaveCount(1);
  });

  test('a MongoDB result switches between the Table and the JSON view; Redis has no switch', async ({ page }) => {
    await setup(page);
    await newTab(page, 'docs');
    await runLine(page, 'db.orders.find({})');
    await expect(byId(page, 'result-grid')).toBeVisible();
    const table = page.getByRole('button', { name: 'Table', exact: true });
    const json = page.getByRole('button', { name: 'JSON', exact: true });
    await expect(table).toHaveAttribute('aria-pressed', 'true');

    await json.click();
    await expect(json).toHaveAttribute('aria-pressed', 'true');
    await expect(byId(page, 'result-grid')).toHaveCount(0);
    const docs = page.getByTestId('document');
    await expect(docs).toHaveCount(3);
    // 2-space indent, one field per line.
    await expect(docs.first()).toContainText('"_id":');
    await expect(docs.first().locator('pre')).toHaveText(/^\{\n {2}"_id": /);
    // Rebuilt from the flattened columns (one per top-level field, `_id` first), not from a `document` column.
    await expect(docs.first().locator('pre')).toContainText('"customer": "ada"');
    await expect(docs.first().locator('pre')).toContainText('"total": 19.9');
    await expect(docs.first().locator('pre')).not.toContainText('"document"');

    await table.click();
    await expect(byId(page, 'result-grid')).toBeVisible();
    await expect(docs).toHaveCount(0);

    await newTab(page, 'cache');
    await runLine(page, 'GET a');
    await expect(byId(page, 'result-grid')).toBeVisible();
    await expect(page.getByRole('button', { name: 'JSON', exact: true })).toHaveCount(0);
  });

  test('Select top 1000 on a MongoDB collection opens a find().limit() query that returns the documents', async ({ page }) => {
    const backend = await setup(page);
    await expandRow(page, 'docs');
    await expandRow(page, 'shop');
    await expandRow(page, 'Collections');
    await treeRow(page, 'orders').click({ button: 'right' });
    await page.getByRole('menuitem', { name: 'Select top 1000' }).click();

    await expect(editor(page)).toContainText('db.orders.find({}).limit(1000)');
    await editor(page).click();
    await page.keyboard.press('Control+Enter');
    await expect(byId(page, 'result-grid')).toBeVisible();
    await expect(page.getByRole('button', { name: 'JSON', exact: true })).toBeVisible();
    await expect(page.getByText('3 rows')).toBeVisible();
    // The tab is bound to the collection's database.
    expect(backend.calls.filter((c) => c.cmd === 'query_plan').at(-1)?.args).toMatchObject({
      connId: ids.legacy,
      database: 'shop',
      text: 'db.orders.find({}).limit(1000)',
    });
  });

  test('FLUSHDB on Production asks for the connection name before it runs', async ({ page }) => {
    const backend = await setup(page);
    await newTab(page, 'cache-prod');
    await runLine(page, 'FLUSHDB');

    const dialog = byId(page, 'run-confirm');
    await expect(dialog).toBeVisible();
    await expect(dialog.getByTestId('run-confirm-row')).toContainText('ADMIN');
    await expect(dialog.getByTestId('run-confirm-row')).toContainText('FLUSHDB');
    await expect(dialog.getByText('cache-prod', { exact: true }).first()).toBeVisible();
    await expect(byId(page, 'run-confirm-run')).toBeDisabled();
    await byId(page, 'run-confirm-name').fill('cache-prod');
    await expect(byId(page, 'run-confirm-run')).toBeEnabled();
    await byId(page, 'run-confirm-cancel').click();
    await expect(dialog).toBeHidden();
    // Cancelled: nothing ran, the data is intact.
    expect(backend.calls.filter((c) => c.cmd === 'query_run')).toEqual([]);
    expect(backend.command.redis.get(0)?.size).toBe(5);
  });

  test('a MongoDB tab suggests the collections after db. and the sampled fields in a filter', async ({ page }) => {
    const backend = await setup(page);
    await newTab(page, 'docs');
    const suggestion = (label: string) =>
      page.getByTestId('query-editor').locator('.suggest-widget .monaco-list-row .label-name', { hasText: new RegExp(`^${label}$`) });

    await editor(page).click();
    await page.keyboard.type('db.');
    await expect(suggestion('orders')).toBeVisible();
    // Database methods come with the collections; collection methods do not.
    await expect(suggestion('getSiblingDB')).toHaveCount(1);
    await expect(suggestion('insertOne')).toHaveCount(0);
    await page.keyboard.press('Escape');

    await page.keyboard.type('orders.find({');
    // The fields `table_detail` sampled from `shop.orders`.
    await expect(suggestion('status')).toBeVisible();
    await expect(suggestion('customer')).toHaveCount(1);
    expect(backend.calls.filter((c) => c.cmd === 'table_detail').at(-1)?.args).toMatchObject({
      id: ids.legacy,
      obj: { database: 'shop', name: 'orders', kind: 'Collection' },
    });

    await page.keyboard.press('Enter');
    await expect(editor(page)).toContainText('db.orders.find({_id})');

    // A Redis tab gets no suggestions.
    await newTab(page, 'cache');
    await editor(page).click();
    await page.keyboard.type('db.');
    await expect(page.getByTestId('query-editor').locator('.suggest-widget.visible')).toHaveCount(0);
  });
});
