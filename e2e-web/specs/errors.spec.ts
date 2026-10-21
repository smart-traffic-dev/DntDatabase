import { expect, test } from '../src/test';
import { createBackend } from '../src/backend';
import { installBackend } from '../src/shim';
import { byId, fillConnectionDialog } from '../src/app';
import { fail } from '../../ui/src/test/fixtures/data';

// Guards the IPC error contract: a command that fails must reach the UI as its real
// AppError (kind / stage / hint), not as an opaque "internal error". This is why the
// shim rejects with a plain object instead of an Error.
test('a failing command shows the backend AppError, not an internal error', async ({ page }) => {
  const backend = createBackend({
    test_connection: () => {
      throw fail('server_db_missing');
    },
  });
  await installBackend(page, backend.handlers);
  await page.goto('/');

  await fillConnectionDialog(page, {
    name: 'e2e-mysql8',
    engine: 'mysql',
    port: 33080,
    user: 'root',
    password: 'dnt_test',
  });
  await byId(page, 'conn-test').click();

  const result = byId(page, 'test-result');
  await expect(result).toContainText("Unknown database 'shop'");
  // Only rendered when the AppError carried a `stage`.
  await expect(byId(page, 'test-stage')).toContainText('Connect');
  await expect(result).toContainText('Database does not exist.');
});
