import type { Page } from '@playwright/test';
import type { Tree } from '../../ui/src/bindings';
import { config, ids } from '../../ui/src/test/fixtures/data';
import { byId } from '../src/app';
import { createBackend } from '../src/backend';
import { installBackend } from '../src/shim';
import { expect, test } from '../src/test';

// Spec 2026-10-21-ux-redesign §5 and §4 "Narrow groups": the query toolbar is one 40 px row, the
// context pill opens the connection / database picker, results stay collapsed until the first
// run, and a narrow toolbar folds Explain / Format / Limit into `⋯`, then Run into an icon.
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

const toolbarWidth = async (page: Page) => (await page.getByRole('toolbar', { name: 'Query' }).boundingBox())!.width;

test.describe('query toolbar', () => {
  test('one 40 px row; the pill picks the connection; Run turns into Run on PROD', async ({ page }) => {
    await page.setViewportSize({ width: 1400, height: 800 });
    await openQuery(page);
    const toolbar = page.getByRole('toolbar', { name: 'Query' });
    expect((await toolbar.boundingBox())!.height).toBe(40);

    const pill = byId(page, 'query-context');
    await expect(pill).toHaveText(/Choose a connection/);
    await expect(byId(page, 'query-run')).toBeDisabled();
    await pill.click();
    const picker = page.getByRole('dialog', { name: 'Connection and database' });
    await expect(byId(page, 'query-connection')).toBeFocused();
    await byId(page, 'query-connection').selectOption({ label: 'shop-prod' });
    await page.keyboard.press('Escape');
    await expect(picker).toBeHidden();
    await expect(pill).toBeFocused();

    await expect(byId(page, 'query-context-env')).toHaveText('PROD');
    await expect(pill).toContainText('shop-prod');
    await expect(byId(page, 'query-run')).toContainText('Run on PROD');
    await expect(byId(page, 'query-run')).toHaveClass(/danger-outline/);

    // The Run menu holds the moved controls.
    await byId(page, 'query-run-menu').click();
    for (const id of ['query-run-all', 'query-run-selection', 'query-continue-on-error', 'query-add-favorite']) {
      await expect(byId(page, id)).toBeVisible();
    }
    await page.keyboard.press('Escape');
    await expect(byId(page, 'query-run-all')).toBeHidden();
  });

  test('results are collapsed to their tab bar with the run keys', async ({ page }) => {
    await openQuery(page);
    const panel = byId(page, 'result-panel');
    await expect(panel).toHaveClass(/collapsed/);
    expect((await panel.boundingBox())!.height).toBeLessThanOrEqual(33);
    await expect(byId(page, 'result-empty-keys')).toContainText('Run statement');
    await expect(page.getByRole('separator', { name: 'Resize editor and results' })).toHaveCount(0);
  });

  test('a narrow toolbar folds Explain, Format and Limit into the more menu, then Run into an icon', async ({ page }) => {
    await page.setViewportSize({ width: 1400, height: 800 });
    await openQuery(page);
    await byId(page, 'query-context').click();
    await byId(page, 'query-connection').selectOption({ label: 'shop-prod' });
    await page.keyboard.press('Escape');
    expect(await toolbarWidth(page)).toBeGreaterThanOrEqual(560);
    await expect(byId(page, 'query-format')).toBeVisible();
    await expect(byId(page, 'query-more')).toHaveCount(0);

    // The sidebar is 280 px by default: an 800 px window leaves the toolbar under 560 px.
    await page.setViewportSize({ width: 800, height: 800 });
    await expect.poll(() => toolbarWidth(page)).toBeLessThan(560);
    await expect(byId(page, 'query-more')).toBeVisible();
    for (const id of ['query-explain', 'query-format', 'query-row-limit']) await expect(byId(page, id)).toHaveCount(0);
    await expect(byId(page, 'query-run')).toContainText('Run');

    await page.setViewportSize({ width: 640, height: 800 });
    await expect.poll(() => toolbarWidth(page)).toBeLessThan(400);
    await expect(byId(page, 'query-run')).toHaveAttribute('aria-label', 'Run on PROD');
    await expect(byId(page, 'query-run')).not.toContainText('Run');
    // The environment badge never folds away.
    await expect(byId(page, 'query-context-env')).toBeVisible();
    expect((await page.getByRole('toolbar', { name: 'Query' }).boundingBox())!.height).toBe(40);
  });
});
