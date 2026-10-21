import type { Page } from '@playwright/test';
import { config, ids } from '../../ui/src/test/fixtures/data';
import { byId, expandRow, treeRow } from '../src/app';
import { createBackend } from '../src/backend';
import { installBackend } from '../src/shim';
import { expect, test } from '../src/test';

// The table designer (P4b) against the canned MySQL 8 `orders` design: open from the tree,
// edit `total`'s precision, review the SQL, apply. The fake backend renders the one
// statement the flow needs; real DDL is covered by the Rust tests.

async function openDesigner(page: Page, name: string, env: 'production' | 'staging') {
  const backend = createBackend();
  // Seeded through the backend itself, so it knows the connection when the tree opens it.
  await backend.handlers.create_connection!({ cfg: config(ids.prod, name, { env_tag: env, default_db: 'dnt_fixture' }).config });
  backend.calls.length = 0;
  await installBackend(page, backend.handlers);
  await page.goto('/');
  await expandRow(page, name);
  await expandRow(page, 'dnt_fixture');
  await expandRow(page, 'Tables');
  await treeRow(page, 'orders').click({ button: 'right' });
  await page.getByRole('menuitem', { name: 'Design table' }).click();
  await expect(byId(page, 'designer-tab')).toBeVisible();
  return backend;
}

/** Selects `total` and sets its precision to 14 in the inspector. */
async function changeTotalPrecision(page: Page) {
  const rows = page.getByRole('table', { name: 'Columns' }).locator('[data-column]');
  await expect(rows).toHaveCount(8);
  await page.locator('[data-column="total"]').getByRole('cell', { name: 'State' }).click();
  const precision = page.getByRole('complementary', { name: 'Column inspector' }).getByRole('spinbutton', { name: 'Precision' });
  await precision.fill('14');
}

test.describe('table designer (browser, fake IPC backend)', () => {
  test('Design table on orders: edit total, review the SQL, apply, the tab is clean again', async ({ page }) => {
    await openDesigner(page, 'shop-staging', 'staging');
    await changeTotalPrecision(page);

    const sql = page.getByRole('region', { name: 'SQL preview' });
    await expect(sql).toContainText('MODIFY COLUMN `total` decimal(14,2)');
    await expect(page.getByText('1 unsaved change')).toBeVisible();

    await page.getByRole('button', { name: /Review & apply/ }).click();
    const dialog = page.getByRole('dialog', { name: 'Review & apply' });
    await expect(dialog).toContainText('MODIFY COLUMN `total` decimal(14,2)');
    await dialog.getByRole('button', { name: 'Apply', exact: true }).click();

    await expect(page.getByText('Changes applied.')).toBeVisible();
    await expect(dialog).toBeHidden();
    // Reloaded from the (updated) base: nothing unsaved, nothing left to review.
    await expect(page.getByText(/unsaved change/)).toHaveCount(0);
    await expect(page.getByRole('button', { name: /Review & apply/ })).toBeDisabled();
    await expect(page.locator('[data-column="total"]').getByRole('textbox', { name: 'Name' })).toHaveValue('total');
  });

  test('PROD connection: Apply stays disabled until the exact table name is typed', async ({ page }) => {
    const backend = await openDesigner(page, 'shop-prod', 'production');
    await changeTotalPrecision(page);

    await page.getByRole('button', { name: /Review & apply/ }).click();
    const dialog = page.getByRole('dialog', { name: 'Review & apply' });
    const apply = dialog.getByRole('button', { name: 'Apply', exact: true });
    const confirm = dialog.getByRole('textbox', { name: 'Confirmation name' });
    await expect(confirm).toBeVisible();
    await expect(apply).toBeDisabled();

    await confirm.fill('order');
    await expect(apply).toBeDisabled();
    await confirm.fill('orders');
    await expect(apply).toBeEnabled();
    await apply.click();

    await expect(page.getByText('Changes applied.')).toBeVisible();
    const sent = backend.calls.find((c) => c.cmd === 'table_design_apply');
    expect(sent?.args).toMatchObject({ confirmed: true, name: 'orders' });
  });
});
