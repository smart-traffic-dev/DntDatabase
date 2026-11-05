import { expect, test } from '../src/test';
import { createBackend } from '../src/backend';
import { installBackend } from '../src/shim';
import { byId, saveConnectionDialog, treeRow } from '../src/app';

test.describe('MongoDB and Redis connections (browser, fake IPC backend)', () => {
  test('a Redis and a MongoDB connection are created in the dialog and shown in the tree', async ({ page }) => {
    const backend = createBackend();
    await installBackend(page, backend.handlers);
    await page.goto('/');

    // Redis: its own port, an optional username, a database index.
    await byId(page, 'new-connection').click();
    await byId(page, 'conn-engine-redis').check();
    await expect(byId(page, 'conn-port')).toHaveValue('6379');
    await expect(page.getByLabel('Username (optional)')).toHaveValue('');
    await byId(page, 'conn-host').fill('127.0.0.1');
    await page.getByLabel('Database index').fill('2');
    await byId(page, 'conn-name').fill('e2e-redis');
    await saveConnectionDialog(page);

    // MongoDB, pasted: the connection string fills the fields.
    await byId(page, 'new-connection').click();
    await byId(page, 'conn-engine-mongo').check();
    await byId(page, 'conn-url-toggle').click();
    await page.getByLabel('Connection string').fill('mongodb://app@db1.local:27018,db2.local:27019/shop?authSource=admin&replicaSet=rs0');
    await expect(byId(page, 'parse-result')).toContainText('Parsed · MongoDB · db1.local:27018');
    await expect(page.getByLabel('Auth database')).toHaveValue('admin');
    await expect(page.getByLabel('Replica set')).toHaveValue('rs0');
    await expect(page.getByLabel('Additional host 1', { exact: true })).toHaveValue('db2.local');    await byId(page, 'conn-name').fill('e2e-mongo');
    await saveConnectionDialog(page);

    await expect(treeRow(page, 'e2e-redis').locator('.chip')).toHaveText('RD');
    await expect(treeRow(page, 'e2e-mongo').locator('.chip')).toHaveText('MG');

    const created = backend.calls.filter((c) => c.cmd === 'create_connection').map((c) => c.args.cfg);
    expect(created).toMatchObject([
      { kind: 'redis', port: 6379, username: '', ssl: { mode: 'disable' }, options: { driver: { kind: 'redis', db: 2 } } },
      {
        kind: 'mongo',
        host: 'db1.local',
        port: 27018,
        username: 'app',
        default_db: 'shop',
        ssl: { mode: 'disable' },
        options: {
          driver: {
            kind: 'mongo',
            srv: false,
            auth_source: 'admin',
            replica_set: 'rs0',
            extra_hosts: [{ host: 'db2.local', port: 27019 }],
          },
        },
      },
    ]);
  });
});
