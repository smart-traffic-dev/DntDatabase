import { expect, test } from '../src/test';
import { createBackend } from '../src/backend';
import { installBackend } from '../src/shim';
import { expandRow, fillConnectionDialog, openDdl, saveConnectionDialog } from '../src/app';

test.describe('connection tree (browser, fake IPC backend)', () => {
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
