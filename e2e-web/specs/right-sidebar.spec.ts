import { createBackend } from '../src/backend';
import { installBackend } from '../src/shim';
import { expect, test } from '../src/test';

test.describe('right sidebar', () => {
  test('opens, collapses, and Insert from History writes into the focused editor', async ({ page }) => {
    await installBackend(page, createBackend({ completion_scope: () => null }).handlers);
    await page.goto('/');
    await page.getByTestId('new-query').click();

    // Collapsed at first: only the 40 px activity bar is on screen.
    const bar = page.getByRole('navigation', { name: 'Right sidebar' });
    await expect(bar).toBeVisible();
    expect((await bar.boundingBox())?.width).toBe(40);
    await expect(page.getByTestId('history-view')).toHaveCount(0);
    await expect(page.getByRole('button', { name: 'Snippets (coming soon)' })).toBeDisabled();

    // Open from the History icon.
    await bar.getByRole('button', { name: 'History' }).click();
    const history = page.getByTestId('history-view');
    await expect(history).toBeVisible();
    await expect(page.getByTestId('history-row')).toHaveCount(2);
    const panel = page.getByTestId('right-sidebar-panel');
    const width = (await panel.boundingBox())!.width;
    expect(width).toBeGreaterThanOrEqual(260);
    expect(width).toBeLessThanOrEqual(560);

    // The active icon collapses it, Ctrl+Alt+B brings it back.
    await bar.getByRole('button', { name: 'History' }).click();
    await expect(history).toHaveCount(0);
    await page.keyboard.press('Control+Alt+b');
    await expect(history).toBeVisible();

    // Status filter: the failed statement only.
    await page.getByRole('radio', { name: 'Errors' }).click();
    await expect(page.getByTestId('history-row')).toHaveCount(1);
    await page.getByRole('radio', { name: 'All' }).click();

    // Insert the successful statement at the caret of the focused editor.
    await page.getByTestId('history-row').filter({ hasText: 'SELECT 42' }).click();
    const detail = page.getByTestId('history-detail');
    await expect(detail).toContainText('SELECT 42 AS answer');
    await detail.getByRole('button', { name: 'Insert' }).click();
    await expect(page.getByTestId('query-editor')).toContainText('SELECT 42 AS answer');

    // Ctrl+Shift+H puts the keyboard in the search box.
    await page.getByTestId('query-editor').locator('.monaco-editor .view-lines').click();
    await page.keyboard.press('Control+Shift+H');
    await expect(page.getByTestId('history-search')).toBeFocused();
  });
});
