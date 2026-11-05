import { config, ids } from '../../ui/src/test/fixtures/data';
import { expandRow, treeRow } from '../src/app';
import { createBackend } from '../src/backend';
import { installBackend } from '../src/shim';
import { expect, test } from '../src/test';

// The key browser of a Redis database against the fake Command server (spec §7.6): scan, filter,
// edit a hash field, set a TTL and delete the key, each change reviewed before it runs.

const redis = config(ids.legacy, 'cache', {
  kind: 'redis',
  port: 6379,
  username: '',
  env_tag: 'production',
  options: {
    version: 1,
    connect_timeout_s: 10,
    ssh_timeout_s: 10,
    charset: null,
    application_name: null,
    query_timeout_s: 0,
    driver: { kind: 'redis', db: 0 },
  },
}).config;

test('browse db0, filter, edit a hash field, set a TTL and delete the key on Production', async ({ page }) => {
  const backend = createBackend();
  await backend.handlers.create_connection!({ cfg: redis });
  await installBackend(page, backend.handlers);
  await page.goto('/');
  await expandRow(page, 'cache');
  await treeRow(page, 'db0 · 5 keys').dblclick();

  const keys = page.getByRole('listbox', { name: 'Keys' }).getByRole('option');
  await expect(keys).toHaveCount(5);
  await expect(page.getByTestId('keys-count')).toHaveText('5 of 5 keys loaded');
  await expect(page.getByRole('tab', { name: /cache · db0/ })).toBeVisible();

  // Filter: the pattern rescans from the start.
  const pattern = page.getByRole('searchbox', { name: 'Key pattern' });
  await pattern.fill('user:*');
  await pattern.press('Enter');
  await expect(keys).toHaveCount(1);
  await expect(keys.first()).toContainText('user:1');
  await expect(keys.first()).toContainText('Hash');

  // Open the hash and edit a field.
  await keys.first().click();
  const pane = page.getByRole('region', { name: 'Value' });
  await expect(pane.getByRole('cell', { name: 'Ada Lovelace' })).toBeVisible();
  await pane.getByRole('button', { name: 'Edit name' }).click();
  const input = pane.getByRole('textbox', { name: 'Value of name' });
  await input.fill('Ada King');
  await input.press('Enter');

  const review = page.getByRole('dialog', { name: 'Review changes' });
  await expect(review.getByLabel('Summary')).toHaveText('1 command');
  await expect(review).toContainText('HSET user:1 name "Ada King"');
  await expect(review).toContainText('Confirmation required');
  await review.getByRole('button', { name: 'Confirm' }).click();
  await expect(review).toBeHidden();
  await expect(pane.getByRole('cell', { name: 'Ada King' })).toBeVisible();
  expect(backend.command.redis.get(0)?.get('user:1')).toMatchObject({ type: 'hash', value: { name: 'Ada King' } });

  // Set a TTL of 10 minutes.
  await expect(pane.getByTestId('key-ttl')).toHaveText('TTL: no TTL');
  await pane.getByRole('button', { name: 'Set TTL…' }).click();
  const ttl = page.getByRole('dialog', { name: 'Set TTL' });
  await ttl.getByLabel('TTL in seconds').fill('600');
  await ttl.getByRole('button', { name: 'Set TTL' }).click();
  await expect(review).toContainText('PEXPIRE user:1 600000');
  await review.getByRole('button', { name: 'Confirm' }).click();
  await expect(review).toBeHidden();
  await expect(pane.getByTestId('key-ttl')).toHaveText(/TTL: (10m|9m 59s)/);
  await expect(keys.first()).toContainText(/10m|9m 59s/);
  await expect(pane.getByRole('button', { name: 'Persist' })).toBeVisible();

  // Delete the key: Production asks for confirmation; the list updates in place.
  const scans = backend.calls.filter((c) => c.cmd === 'keys_scan').length;
  await pane.getByRole('button', { name: 'Delete key' }).click();
  await expect(review).toContainText('DEL user:1');
  await expect(review.getByRole('note').first()).toContainText('Confirmation required');
  await review.getByRole('button', { name: 'Confirm' }).click();
  await expect(review).toBeHidden();
  await expect(keys).toHaveCount(0);
  await expect(page.getByTestId('keys-count')).toHaveText('0 of 5 keys loaded');
  expect(backend.command.redis.get(0)?.has('user:1')).toBe(false);
  expect(backend.calls.filter((c) => c.cmd === 'keys_scan')).toHaveLength(scans);
});
