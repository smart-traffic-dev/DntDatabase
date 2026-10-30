import type { Page } from '@playwright/test';
import type { Tree } from '../../ui/src/bindings';
import { config, ids } from '../../ui/src/test/fixtures/data';
import { byId } from '../src/app';
import { createBackend } from '../src/backend';
import { installBackend } from '../src/shim';
import { expect, test } from '../src/test';

// The query controls are a toolbar row of their own under the group's tab strip (as in
// Navicat); the context chip opens the connection / database picker; results stay collapsed
// until the first run; under 360 px the toolbar keeps the chip badge, Run / Stop and `⋯`.
// Widths depend on real layout, so these run in Chromium.

const tree = (): Tree => ({
  groups: [],
  connections: [
    {
      config: config(ids.prod, 'shop-prod', { env_tag: 'production', default_db: 'dnt_fixture' }).config,
      has_db_password: true,
      has_ssh_secret: false,
      last_connected_at: null,
    },
  ],
});

async function openQuery(page: Page) {
  await installBackend(page, createBackend({ list_tree: tree }).handlers);
  await page.goto('/');
  await byId(page, 'new-query').click();
  await expect(byId(page, 'query-editor')).toBeVisible();
}

async function bindProd(page: Page) {
  await byId(page, 'query-context').click();
  await byId(page, 'query-connection').selectOption({ label: 'shop-prod' });
  await page.keyboard.press('Escape');
}

const strip = (page: Page) => page.locator('.tabstrip').first();
const toolbarRow = (page: Page) => page.locator('.tabstrip-toolbar').first();
const rowWidth = async (page: Page) => (await toolbarRow(page).boundingBox())!.width;

test.describe('query toolbar row', () => {
  test('the toolbar has its own row under the tabs; the chip picks the connection; Run turns red on PROD', async ({ page }) => {
    await page.setViewportSize({ width: 1400, height: 800 });
    await openQuery(page);
    const toolbar = page.getByRole('toolbar', { name: 'Query' });
    // Not in the tab strip: on the row under it, spanning the group.
    await expect(strip(page).getByRole('toolbar', { name: 'Query' })).toHaveCount(0);
    await expect(toolbarRow(page).getByRole('toolbar', { name: 'Query' })).toHaveCount(1);
    const box = (await toolbar.boundingBox())!;
    const tabs = (await strip(page).boundingBox())!;
    const row = (await toolbarRow(page).boundingBox())!;
    expect(row.y).toBeGreaterThanOrEqual(tabs.y + tabs.height - 1);
    expect(box.width).toBeGreaterThan(row.width - 2);
    // The chip leads the row; ⋯ ends it.
    const chipBox = (await byId(page, 'query-context').boundingBox())!;
    const moreBox = (await byId(page, 'query-more').boundingBox())!;
    expect(chipBox.x - row.x).toBeLessThan(16);
    expect(row.x + row.width - (moreBox.x + moreBox.width)).toBeLessThan(16);

    const chip = byId(page, 'query-context');
    await expect(chip).toHaveText(/Choose a connection/);
    await expect(byId(page, 'query-run')).toBeDisabled();
    await chip.click();
    const picker = page.getByRole('dialog', { name: 'Connection and database' });
    await expect(byId(page, 'query-connection')).toBeFocused();
    await byId(page, 'query-connection').selectOption({ label: 'shop-prod' });
    await page.keyboard.press('Escape');
    await expect(picker).toBeHidden();
    await expect(chip).toBeFocused();

    await expect(byId(page, 'query-context-env')).toHaveText('PROD');
    await expect(chip).toHaveAttribute('title', /shop-prod/);
    await expect(byId(page, 'query-run')).toHaveAttribute('aria-label', 'Run on PROD');
    await expect(byId(page, 'query-run')).toHaveClass(/danger-outline/);

    // The Run menu.
    await byId(page, 'query-run-menu').click();
    for (const id of ['query-run-statement', 'query-run-all', 'query-run-selection', 'query-continue-on-error', 'query-add-favorite']) {
      await expect(byId(page, id)).toBeVisible();
    }
    await page.keyboard.press('Escape');
    await expect(byId(page, 'query-run-all')).toBeHidden();

    // ⋯: Format, the Limit radio list, Auto / Manual commit, Open in new group.
    await byId(page, 'query-more').click();
    for (const id of ['query-format', 'query-row-limit-1000', 'query-tx-auto', 'query-tx-manual', 'query-open-in-group']) {
      await expect(byId(page, id)).toBeVisible();
    }
    await byId(page, 'query-row-limit-1000').click();
    await byId(page, 'query-more').click();
    await expect(byId(page, 'query-row-limit-1000')).toHaveAttribute('aria-checked', 'true');
    await page.keyboard.press('Escape');
  });

  test('a tab that never ran has no results panel; the editor fills the group', async ({ page }) => {
    await openQuery(page);
    await expect(byId(page, 'result-panel')).toHaveCount(0);
    await expect(page.getByRole('separator', { name: 'Resize editor and results' })).toHaveCount(0);
  });

  test('in a group under 360 px the toolbar keeps the chip badge, Run and ⋯; Explain moves into ⋯', async ({ page }) => {
    await page.setViewportSize({ width: 1400, height: 800 });
    await openQuery(page);
    await bindProd(page);
    await expect(byId(page, 'query-explain')).toBeVisible();
    // No database picked: the chip names the connection's default one.
    await expect(byId(page, 'query-context')).toHaveText('PRODshop-prod›dnt_fixture');

    // The sidebar is 280 px by default: a 620 px window leaves the group under 360 px.
    await page.setViewportSize({ width: 620, height: 800 });
    await expect.poll(() => rowWidth(page)).toBeLessThan(360);
    await expect(byId(page, 'query-explain')).toHaveCount(0);
    await expect(byId(page, 'query-context')).toHaveText('PROD');
    await expect(byId(page, 'query-run')).toBeVisible();
    await expect(byId(page, 'query-more')).toBeVisible();
    await byId(page, 'query-more').click();
    await expect(byId(page, 'query-more-explain')).toBeVisible();
    await page.keyboard.press('Escape');
    // The toolbar still fits its row.
    const box = (await page.getByRole('toolbar', { name: 'Query' }).boundingBox())!;
    const row = (await toolbarRow(page).boundingBox())!;
    expect(box.x + box.width).toBeLessThanOrEqual(row.x + row.width + 1);
  });
});
