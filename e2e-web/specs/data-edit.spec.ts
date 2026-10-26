import type { Page } from '@playwright/test';
import type { EditableInfo } from '../../ui/src/bindings';
import { config, ids } from '../../ui/src/test/fixtures/data';
import { byId, expandRow, treeRow } from '../src/app';
import { createBackend } from '../src/backend';
import { installBackend } from '../src/shim';
import { expect, test } from '../src/test';

// In-place editing in the Data tab: the cell editor Glide opens on a double-click or on typing.
// It renders into the page's `#portal` element; without it Glide only logs an error and nothing opens.

const EDITABLE: EditableInfo = {
  editable: true,
  reason: null,
  key: ['id'],
  hidden_key: [],
  editable_columns: ['customer_id', 'total'],
  atomic: 'yes',
  protection: 'snapshot',
};

async function openOrders(page: Page) {
  const backend = createBackend({ data_editable: () => EDITABLE });
  await backend.handlers.create_connection!({ cfg: config(ids.analytics, 'Analytics', { default_db: 'dnt_fixture', env: 'local' }).config });
  await installBackend(page, backend.handlers);
  await page.goto('/');
  await expandRow(page, 'Analytics');
  await expandRow(page, 'dnt_fixture');
  await expandRow(page, 'Tables');
  await treeRow(page, 'orders').dblclick();
  await expect(page.getByRole('toolbar', { name: 'Edit' })).toBeVisible();
}

/** The middle of a data cell: `col` 0 is the first data column, `row` 0 the first row. */
async function cellPoint(page: Page, col: number, row: number) {
  const box = (await byId(page, 'result-grid').locator('canvas').first().boundingBox())!;
  // Columns are as wide as their headers: a point well inside each of `id`, `customer_id`, `total`.
  // Rows are 24 px under a 36 px header (Compact density).
  const x = [70, 160, 270][col]!;
  const y = 48 + row * 24;
  return { x: box.x + x, y: box.y + y };
}

const cellEditor = (page: Page) => page.locator('#portal textarea');

test('double-clicking an editable cell opens the cell editor and Enter stages the change', async ({ page }) => {
  await openOrders(page);
  const p = await cellPoint(page, 2, 0);
  await page.mouse.dblclick(p.x, p.y);
  await expect(cellEditor(page)).toBeVisible();
  await expect(cellEditor(page)).toHaveValue('19.90');
  await cellEditor(page).fill('20.50');
  await page.keyboard.press('Enter');
  await expect(cellEditor(page)).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'Apply changes' })).toBeVisible();
});

test('typing on a selected editable cell starts editing it', async ({ page }) => {
  await openOrders(page);
  const p = await cellPoint(page, 1, 1);
  await page.mouse.click(p.x, p.y);
  await page.keyboard.type('7');
  await expect(cellEditor(page)).toBeVisible();
  await page.keyboard.press('Enter');
  await expect(page.getByRole('button', { name: 'Apply changes' })).toBeVisible();
});
