import { expect, test } from '../src/test';
import { createBackend } from '../src/backend';
import { installBackend } from '../src/shim';
import { byId, expandRow, saveConnectionDialog, treeRow } from '../src/app';

test.describe('tree of Command connections (browser, fake IPC backend)', () => {
  test('a Redis connection lists its databases as keyspace leaves with key counts', async ({ page }) => {
    const backend = createBackend();
    await installBackend(page, backend.handlers);
    await page.goto('/');

    await byId(page, 'new-connection').click();
    await byId(page, 'conn-engine-redis').check();
    await byId(page, 'conn-host').fill('127.0.0.1');
    await byId(page, 'conn-name').fill('e2e-redis');
    await saveConnectionDialog(page);

    await expandRow(page, 'e2e-redis');
    const db0 = treeRow(page, 'db0 · 5 keys');
    const db2 = treeRow(page, 'db2 · 3 keys');
    await expect(db0).toBeVisible();
    await expect(db2).toBeVisible();
    // Leaves: no expander, and nothing is listed below them.
    await expect(db0).not.toHaveAttribute('aria-expanded', /.*/);
    await db0.dblclick();
    await expect(page.getByTestId('tree-row')).toHaveCount(3);
    expect(backend.calls.filter((c) => c.cmd === 'list_objects')).toHaveLength(0);
  });

  test('a MongoDB database has Collections and Views folders; the collections are listed', async ({ page }) => {
    const backend = createBackend();
    await installBackend(page, backend.handlers);
    await page.goto('/');

    await byId(page, 'new-connection').click();
    await byId(page, 'conn-engine-mongo').check();
    await byId(page, 'conn-host').fill('127.0.0.1');
    await byId(page, 'conn-name').fill('e2e-mongo');
    await saveConnectionDialog(page);

    await expandRow(page, 'e2e-mongo');
    await expandRow(page, 'shop');
    await expect(treeRow(page, 'Collections')).toBeVisible();
    await expect(treeRow(page, 'Views')).toBeVisible();
    await expandRow(page, 'Collections');
    await expect(treeRow(page, 'orders')).toBeVisible();
  });
});
