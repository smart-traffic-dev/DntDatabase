import { $, browser, expect } from '@wdio/globals';
import { evidence } from './evidence';

const byId = (id: string) => $(`[data-testid="${id}"]`);

/** Tree row whose label is exactly `label`. */
const treeRow = (label: string) =>
  $(`//*[@data-testid="tree-row"][.//span[contains(@class,"label") and normalize-space(.)="${label}"]]`);

async function expandRow(label: string) {
  const row = await treeRow(label);
  await row.waitForDisplayed({ timeout: 20_000 });
  if ((await row.getAttribute('aria-expanded')) !== 'true') {
    await row.doubleClick();
  }
  await browser.waitUntil(async () => (await row.getAttribute('aria-expanded')) === 'true', {
    timeout: 20_000,
    timeoutMsg: `"${label}" did not expand`,
  });
}

describe('DntDatabase PostgreSQL 17', () => {
  it('connects and shows the DDL of dnt_fixture.public.orders', async () => {
    await byId('new-connection').waitForClickable({ timeout: 30_000 });
    await byId('new-connection').click();

    await byId('conn-name').setValue('e2e-pg17');
    await byId('conn-engine-postgres').click();
    await byId('conn-host').setValue('127.0.0.1');
    await byId('conn-port').setValue('54317');
    await byId('conn-user').setValue('postgres');
    await byId('conn-password').setValue('dnt_test');
    await evidence('pg-01-connection-dialog');

    await byId('conn-test').click();
    const result = await byId('test-result');
    await result.waitForDisplayed({ timeout: 30_000 });
    await expect(result).toHaveText(expect.stringContaining('Connected'));
    await evidence('pg-02-test-connected');

    await byId('conn-save').click();

    await expandRow('e2e-pg17');
    await expandRow('dnt_fixture');
    await expandRow('public');
    await expandRow('Tables');

    const orders = await treeRow('orders');
    await orders.waitForDisplayed({ timeout: 20_000 });
    await evidence('pg-03-tree-expanded');
    await orders.doubleClick();
    await evidence('pg-04-object-tab');

    const ddlTab = await byId('object-tab-ddl');
    await ddlTab.waitForClickable({ timeout: 20_000 });
    await ddlTab.click();

    const ddl = await byId('object-ddl-text');
    await ddl.waitForDisplayed({ timeout: 20_000 });
    await expect(ddl).toHaveText(expect.stringContaining('CREATE TABLE'));
    await evidence('pg-05-ddl');
  });
});
