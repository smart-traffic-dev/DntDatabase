import { expect, test } from '../src/test';
import { createBackend } from '../src/backend';
import { installBackend } from '../src/shim';

test.describe('editor groups', () => {
  test('Ctrl+\\ splits the active query into a second group; Ctrl+K 1 merges back', async ({ page }) => {
    await installBackend(page, createBackend().handlers);
    await page.goto('/');
    await page.getByTestId('new-query').click();
    await page.getByTestId('new-query').click();
    await expect(page.getByTestId('editor-group')).toHaveCount(1);

    await page.keyboard.press('Control+Backslash');
    await expect(page.getByTestId('editor-group')).toHaveCount(2);
    const [left, right] = [page.getByTestId('editor-group').nth(0), page.getByTestId('editor-group').nth(1)];
    await expect(left.getByRole('tablist', { name: 'Open tabs' }).getByRole('tab')).toHaveCount(1);
    await expect(right.getByRole('tablist', { name: 'Open tabs' }).getByRole('tab')).toHaveCount(1);
    await expect(right).toHaveClass(/focused/);

    // Both panes are on screen side by side.
    const [a, b] = [await left.boundingBox(), await right.boundingBox()];
    expect(a && b && a.x + a.width <= b.x + 1).toBe(true);

    await page.keyboard.press('Control+k');
    await page.keyboard.press('1');
    await expect(page.getByTestId('editor-group')).toHaveCount(1);
    await expect(page.getByTestId('editor-group').getByRole('tablist', { name: 'Open tabs' }).getByRole('tab')).toHaveCount(2);
  });
});
