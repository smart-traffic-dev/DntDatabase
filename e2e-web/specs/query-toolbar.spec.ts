import type { Page } from '@playwright/test';
import type { Tree } from '../../ui/src/bindings';
import { config, ids } from '../../ui/src/test/fixtures/data';
import { byId } from '../src/app';
import { createBackend } from '../src/backend';
import { installBackend } from '../src/shim';
import { expect, test } from '../src/test';

// Spec 2026-10-21-editor-v2 §2: the query controls are an action cluster at the right end of the
// group's tab strip (no toolbar row); the context chip opens the connection / database picker;
// results stay collapsed until the first run; under 360 px the cluster keeps the chip badge,
// Run / Stop and `⋯`. Widths depend on real layout, so these run in Chromium.

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
const stripWidth = async (page: Page) => (await strip(page).boundingBox())!.width;

test.describe('query toolbar in the tab strip', () => {
  test('the cluster sits in the strip; the chip picks the connection; Run turns red on PROD', async ({ page }) => {
    await page.setViewportSize({ width: 1400, height: 800 });
    await openQuery(page);
    const toolbar = page.getByRole('toolbar', { name: 'Query' });
    // Inside the tab strip, at its right end: no 40 px row of its own.
    await expect(strip(page).getByRole('toolbar', { name: 'Query' })).toHaveCount(1);
    const box = (await toolbar.boundingBox())!;
    const row = (await strip(page).boundingBox())!;
    expect(box.y).toBeGreaterThanOrEqual(row.y);
    expect(box.y + box.height).toBeLessThanOrEqual(row.y + row.height);
    expect(row.x + row.width - (box.x + box.width)).toBeLessThan(16);

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

  test('results are collapsed to their tab bar with the run keys', async ({ page }) => {
    await openQuery(page);
    const panel = byId(page, 'result-panel');
    await expect(panel).toHaveClass(/collapsed/);
    expect((await panel.boundingBox())!.height).toBeLessThanOrEqual(33);
    await expect(byId(page, 'result-empty-keys')).toContainText('Run statement');
    await expect(page.getByRole('separator', { name: 'Resize editor and results' })).toHaveCount(0);
  });

  test('in a group under 360 px the cluster keeps the chip badge, Run and ⋯; Explain moves into ⋯', async ({ page }) => {
    await page.setViewportSize({ width: 1400, height: 800 });
    await openQuery(page);
    await bindProd(page);
    await expect(byId(page, 'query-explain')).toBeVisible();
    await expect(byId(page, 'query-context')).toHaveText('PRODDefault database');

    // The sidebar is 280 px by default: a 620 px window leaves the group under 360 px.
    await page.setViewportSize({ width: 620, height: 800 });
    await expect.poll(() => stripWidth(page)).toBeLessThan(360);
    await expect(byId(page, 'query-explain')).toHaveCount(0);
    await expect(byId(page, 'query-context')).toHaveText('PROD');
    await expect(byId(page, 'query-run')).toBeVisible();
    await expect(byId(page, 'query-more')).toBeVisible();
    await byId(page, 'query-more').click();
    await expect(byId(page, 'query-more-explain')).toBeVisible();
    await page.keyboard.press('Escape');
    // The cluster still fits the strip, at its right end.
    const box = (await page.getByRole('toolbar', { name: 'Query' }).boundingBox())!;
    const row = (await strip(page).boundingBox())!;
    expect(box.x + box.width).toBeLessThanOrEqual(row.x + row.width + 1);
  });
});
