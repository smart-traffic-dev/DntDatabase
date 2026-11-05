import type { Page } from '@playwright/test';
import { config, ids } from '../../ui/src/test/fixtures/data';
import { byId, expandRow, treeRow } from '../src/app';
import { createBackend } from '../src/backend';
import { installBackend } from '../src/shim';
import { expect, test } from '../src/test';

// The Data tab of a MongoDB collection against the fake Command server: one column per top-level
// field, a JSON filter sent as `raw_json`, and an edit previewed as an `updateOne` command.

const mongo = config(ids.legacy, 'docs', {
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

async function openOrders(page: Page) {
  const backend = createBackend();
  await backend.handlers.create_connection!({ cfg: mongo });
  await installBackend(page, backend.handlers);
  await page.goto('/');
  await expandRow(page, 'docs');
  await expandRow(page, 'shop');
  await expandRow(page, 'Collections');
  await treeRow(page, 'orders').dblclick();
  await expect(page.getByRole('toolbar', { name: 'Edit' })).toBeVisible();
  return backend;
}

/** Selects the first data cell (`_id` of the first row): right of the 32 px row marker, under the 36 px header. */
async function selectFirstCell(page: Page) {
  const box = (await byId(page, 'result-grid').locator('canvas').first().boundingBox())!;
  await page.mouse.click(box.x + 44, box.y + 48);
}

test('a JSON filter narrows the documents; an edit is previewed as updateOne, applied and reloaded', async ({ page }) => {
  const backend = await openOrders(page);
  const pager = page.getByTestId('data-pager');
  await expect(pager).toContainText('1–3');
  await expect(page.getByText('Changes are applied one document at a time.')).toBeVisible();

  // Filter: the raw mode is a JSON filter on this connection.
  await page.getByRole('toolbar', { name: 'Object' }).getByRole('button', { name: 'Filter', exact: true }).click();
  const bar = page.getByRole('search', { name: 'Filter' });
  await bar.getByRole('button', { name: 'JSON filter' }).click();
  const input = bar.getByRole('textbox', { name: 'JSON filter' });
  await expect(input).toHaveAttribute('placeholder', '{ "status": "paid" }');
  await input.fill('{"status":"paid"}');
  await input.press('Enter');
  await expect(pager).toContainText('1–1');
  expect(backend.calls.filter((c) => c.cmd === 'table_data').at(-1)?.args.filter).toEqual({ mode: 'and', conditions: [], raw_json: '{"status":"paid"}' });

  // Edit `customer` of o1: the cell right of `_id`.
  await selectFirstCell(page);
  await page.keyboard.press('ArrowRight');
  await page.keyboard.type('e');
  const cellEditor = page.locator('#portal textarea');
  await expect(cellEditor).toBeVisible();
  await cellEditor.fill('eve');
  await page.keyboard.press('Enter');
  await expect(cellEditor).toHaveCount(0);
  await page.getByRole('button', { name: 'Apply changes' }).click();

  const dialog = page.getByRole('dialog', { name: 'Review changes' });
  await expect(dialog.getByLabel('Summary')).toHaveText('1 command');
  await expect(dialog).toContainText('db.orders.updateOne({ "_id": "o1" }, { "$set": { "customer": "eve" } })');
  const loads = backend.calls.filter((c) => c.cmd === 'table_data').length;
  await dialog.getByRole('button', { name: 'Confirm' }).click();
  await expect(dialog).toBeHidden();

  // Applied, then the page is read again and shows the stored value.
  expect(backend.command.mongo.get('shop')?.get('orders')?.[0]).toMatchObject({ _id: 'o1', customer: 'eve' });
  await expect.poll(() => backend.calls.filter((c) => c.cmd === 'table_data').length).toBeGreaterThan(loads);
  await expect(page.getByRole('button', { name: 'Apply changes' })).toHaveCount(0);
  await selectFirstCell(page);
  await page.keyboard.press('Shift+Space');
  const record = byId(page, 'record-view');
  await expect(record.getByRole('region', { name: 'customer' }).locator('.gt-field-value')).toHaveText('eve');
});
