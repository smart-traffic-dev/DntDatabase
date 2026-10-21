import { byId } from '../src/app';
import { createBackend } from '../src/backend';
import { installBackend } from '../src/shim';
import { expect, test } from '../src/test';

// Review finding: Monaco's addCommand is global (the last mounted editor wins), so with two query
// tabs side by side Ctrl+Enter ran the other tab. The run keys must act on the editor they are
// pressed in. Unbound tabs answer a run with "Choose a connection first." in their own Messages,
// which shows which tab the key reached without a database.
test('Ctrl+Enter runs the query of the group it is pressed in', async ({ page }) => {
  await installBackend(page, createBackend({ completion_scope: () => null }).handlers);
  await page.goto('/');

  const editors = page.getByTestId('query-editor');
  const typeInto = async (index: number, sql: string) => {
    await editors.nth(index).locator('.monaco-editor .view-lines').click();
    await page.keyboard.type(sql);
    await page.keyboard.press('Escape'); // close the suggestion list
  };

  await byId(page, 'new-query').click();
  await typeInto(0, 'SELECT 111');
  await byId(page, 'new-query').click();
  await typeInto(0, 'SELECT 222');
  // Query 2 into its own group, on the right (its editor mounts last).
  await page.keyboard.press('Control+Backslash');
  await expect(page.getByTestId('editor-group')).toHaveCount(2);
  await expect(editors).toHaveCount(2);

  const [left, right] = [page.getByTestId('editor-group').nth(0), page.getByTestId('editor-group').nth(1)];
  await editors.nth(0).locator('.monaco-editor .view-lines').click();
  await page.keyboard.press('Control+Enter');
  await expect(left.getByText('Choose a connection first.')).toBeVisible();
  await expect(right.getByText('Choose a connection first.')).toHaveCount(0);

  // And the other way round.
  await editors.nth(1).locator('.monaco-editor .view-lines').click();
  await page.keyboard.press('Control+Enter');
  await expect(right.getByText('Choose a connection first.')).toBeVisible();
});
