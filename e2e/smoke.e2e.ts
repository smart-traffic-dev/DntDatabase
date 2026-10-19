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

describe('DntDatabase smoke', () => {
  it('connects to MySQL 8 and shows the DDL of dnt_fixture.orders', async () => {
    await byId('new-connection').waitForClickable({ timeout: 30_000 });
    await evidence('01-app-start');
    await byId('new-connection').click();

    await byId('conn-name').setValue('e2e-mysql8');
    await byId('conn-engine-mysql').click();
    await byId('conn-host').setValue('127.0.0.1');
    await byId('conn-port').setValue('33080');
    await byId('conn-user').setValue('root');
    await byId('conn-password').setValue('dnt_test');
    await evidence('02-connection-dialog');

    await byId('conn-test').click();
    const result = await byId('test-result');
    await result.waitForDisplayed({ timeout: 30_000 });
    await expect(result).toHaveText(expect.stringContaining('Connected'));
    await evidence('03-test-connected');

    await byId('conn-save').click();

    await expandRow('e2e-mysql8');
    await expandRow('dnt_fixture');
    await expandRow('Tables');

    const orders = await treeRow('orders');
    await orders.waitForDisplayed({ timeout: 20_000 });
    await evidence('04-tree-expanded');
    await orders.doubleClick();
    await evidence('05-object-tab');

    const ddlTab = await byId('object-tab-ddl');
    await ddlTab.waitForClickable({ timeout: 20_000 });
    await ddlTab.click();

    const ddl = await byId('object-ddl-text');
    await ddl.waitForDisplayed({ timeout: 20_000 });
    await expect(ddl).toHaveText(expect.stringContaining('CREATE TABLE'));
    await evidence('06-ddl');
  });
});
