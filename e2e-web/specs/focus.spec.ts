import { expect, test } from '../src/test';
import { createBackend } from '../src/backend';
import { installBackend } from '../src/shim';
import { byId } from '../src/app';

// Spec 2026-10-21-ux-redesign §2.3: text fields show keyboard focus with an accent border, every other
// control with a 1px accent outline. These depend on the real cascade, so they run in Chromium.
const focusColor = (page: import('@playwright/test').Page) =>
  page.evaluate(() => {
    const probe = document.createElement('span');
    probe.style.color = 'var(--dnt-focus)';
    document.body.appendChild(probe);
    const c = getComputedStyle(probe).color;
    probe.remove();
    return c;
  });

test.describe('keyboard focus is visible', () => {
  test.beforeEach(async ({ page }) => {
    await installBackend(page, createBackend().handlers);
    await page.goto('/');
  });

  test('dialog text field gets an accent border', async ({ page }) => {
    await byId(page, 'new-connection').click();
    const name = byId(page, 'conn-name');
    await name.focus();
    await expect.poll(() => name.evaluate((el) => getComputedStyle(el).borderTopColor)).toBe(await focusColor(page));
  });

  test('sidebar filter gets an accent border', async ({ page }) => {
    const filter = page.getByPlaceholder('Filter objects…');
    await filter.focus();
    await expect.poll(() => filter.evaluate((el) => getComputedStyle(el).borderTopColor)).toBe(await focusColor(page));
  });

  test('checkbox keeps a visible focus outline', async ({ page }) => {
    await byId(page, 'new-connection').click();
    const box = page.getByRole('checkbox').first();
    await page.keyboard.press('Shift');
    await box.focus();
    const style = await box.evaluate((el) => {
      const s = getComputedStyle(el);
      return { style: s.outlineStyle, width: s.outlineWidth, color: s.outlineColor };
    });
    expect(style.style).not.toBe('none');
    expect(style.width).toBe('1px');
    expect(style.color).toBe(await focusColor(page));
  });
});
