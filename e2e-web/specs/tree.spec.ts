import { expect, test } from '../src/test';
import { createBackend } from '../src/backend';
import { installBackend } from '../src/shim';
import { expandRow, fillConnectionDialog, openDdl, saveConnectionDialog } from '../src/app';

test.describe('connection tree (browser, fake IPC backend)', () => {
  test('right-click on the empty area offers New group; the group is created at the top level', async ({ page }) => {
    const backend = createBackend();
    await installBackend(page, backend.handlers);
    await page.goto('/');
    await fillConnectionDialog(page, { name: 'e2e-mysql8', engine: 'mysql', port: 33080, user: 'root', password: 'dnt_test' });
    await saveConnectionDialog(page);

    // Below the last row: the tree's empty area.
    const last = page.getByTestId('tree-row').last();
    const box = (await last.boundingBox())!;
    await page.mouse.click(box.x + 40, box.y + box.height + 60, { button: 'right' });
    const menu = page.getByRole('menu', { name: 'Connections actions' });
    await expect(menu.getByRole('menuitem')).toHaveText(['New connection…Ctrl+Shift+N', 'New group', 'Import connections…', 'Export connections…']);
    await menu.getByRole('menuitem', { name: 'New group' }).click();

    const dialog = page.getByRole('dialog', { name: 'New group' });
    await dialog.getByLabel('Group name').fill('Reporting');
    await dialog.getByRole('button', { name: 'Create' }).click();
    await expect(dialog).toBeHidden();
    await expect(page.getByTestId('tree-row').filter({ hasText: 'Reporting' })).toBeVisible();
    expect(backend.calls.find((c) => c.cmd === 'create_group')?.args).toEqual({ name: 'Reporting', parentId: null });
  });

  test('MySQL 8: connects and shows the DDL of dnt_fixture.orders', async ({ page }) => {
    await installBackend(page, createBackend().handlers);
    await page.goto('/');

    await fillConnectionDialog(page, {
      name: 'e2e-mysql8',
      engine: 'mysql',
      port: 33080,
      user: 'root',
      password: 'dnt_test',
    });

    await page.getByTestId('conn-test').click();
    await expect(page.getByTestId('test-result')).toContainText('Connected');
    await saveConnectionDialog(page);

    await expandRow(page, 'e2e-mysql8');
    await expandRow(page, 'dnt_fixture');
    await expandRow(page, 'Tables');
    await openDdl(page, 'orders');
  });

  test('PostgreSQL 17: connects through the schema and shows the DDL of public.orders', async ({ page }) => {
    await installBackend(page, createBackend().handlers);
    await page.goto('/');

    await fillConnectionDialog(page, {
      name: 'e2e-pg17',
      engine: 'postgres',
      port: 54317,
      user: 'postgres',
      password: 'dnt_test',
    });

    await page.getByTestId('conn-test').click();
    await expect(page.getByTestId('test-result')).toContainText('Connected');
    await saveConnectionDialog(page);

    await expandRow(page, 'e2e-pg17');
    await expandRow(page, 'dnt_fixture');
    await expandRow(page, 'public');
    await expandRow(page, 'Tables');
    await openDdl(page, 'orders');
  });
});
