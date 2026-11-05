import type { Page } from '@playwright/test';
import { byId, expandRow, fillConnectionDialog, saveConnectionDialog, treeRow } from '../src/app';
import { createBackend } from '../src/backend';
import { installBackend } from '../src/shim';
import { expect, test } from '../src/test';

// Editor v2 §1: tabs drag like VS Code (pane edges split, the middle and the strips move), every
// splitter resizes its own split, and a single click in the tree opens a preview tab.

const groups = (page: Page) => page.getByTestId('editor-group');
const stripTabs = (page: Page, group: number) =>
  groups(page).nth(group).getByRole('tablist', { name: 'Open tabs' }).getByRole('tab');
const titles = async (page: Page) => {
  const out: string[][] = [];
  for (let i = 0; i < (await groups(page).count()); i++) out.push(await stripTabs(page, i).allTextContents());
  return out.map((g) => g.map((t) => t.replace(/^SQL/, '').trim()));
};

async function threeQueries(page: Page) {
  await installBackend(page, createBackend({ completion_scope: () => null }).handlers);
  await page.goto('/');
  for (let i = 0; i < 3; i++) await byId(page, 'new-query').click();
  // Query 3 into a group on the right.
  await page.keyboard.press('Control+Backslash');
  await expect(groups(page)).toHaveCount(2);
}

test('a tab dropped on the left edge of another pane splits that pane on its left', async ({ page }) => {
  await threeQueries(page);
  const target = groups(page).nth(1).getByTestId('group-content');
  await stripTabs(page, 0).filter({ hasText: 'Query 1' }).dragTo(target, { targetPosition: { x: 20, y: 200 } });
  await expect(groups(page)).toHaveCount(3);
  expect(await titles(page)).toEqual([['Query 2'], ['Query 1'], ['Query 3']]);
});

test('a tab dropped on another strip moves there; the emptied group closes', async ({ page }) => {
  await threeQueries(page);
  await page.keyboard.press('Control+Alt+1');
  const target = groups(page).nth(1).getByRole('tablist', { name: 'Open tabs' });
  await stripTabs(page, 0).filter({ hasText: 'Query 1' }).dragTo(target);
  await stripTabs(page, 0).filter({ hasText: 'Query 2' }).dragTo(groups(page).nth(1).getByRole('tablist', { name: 'Open tabs' }));
  await expect(groups(page)).toHaveCount(1);
  expect((await titles(page))[0]!.sort()).toEqual(['Query 1', 'Query 2', 'Query 3']);
});

test('dragging a splitter resizes the two panes it divides', async ({ page }) => {
  await threeQueries(page);
  const before = (await groups(page).nth(0).boundingBox())!.width;
  const bar = page.getByRole('separator', { name: 'Resize editor groups' });
  const box = (await bar.boundingBox())!;
  await page.mouse.move(box.x + box.width / 2, box.y + 200);
  await page.mouse.down();
  await page.mouse.move(box.x + box.width / 2 - 150, box.y + 200, { steps: 5 });
  await page.mouse.up();
  const after = (await groups(page).nth(0).boundingBox())!.width;
  expect(before - after).toBeGreaterThan(100);
  await bar.dblclick();
  expect(Math.abs((await groups(page).nth(0).boundingBox())!.width - before)).toBeLessThan(4);
});

test('a single click shows a table in Object info and a routine in a preview tab; a double click keeps a tab', async ({ page }) => {
  await installBackend(page, createBackend().handlers);
  await page.goto('/');
  await fillConnectionDialog(page, { name: 'e2e-mysql8', engine: 'mysql', port: 33080, user: 'root', password: 'dnt_test' });
  await saveConnectionDialog(page);
  await expandRow(page, 'e2e-mysql8');
  await expandRow(page, 'dnt_fixture');
  await expandRow(page, 'Tables');
  // A table: the Object info panel, no tab.
  await treeRow(page, 'orders').click();
  await expect(page.getByTestId('object-info-view')).toContainText('orders');
  await expect(page.locator('.tab')).toHaveCount(0);
  // A routine: a preview tab.
  await expandRow(page, 'Procedures');
  await treeRow(page, 'p_restock').click();
  await expect(stripTabs(page, 0)).toHaveCount(1);
  await expect(page.locator('.tab.preview')).toHaveCount(1);
  // A double click on the table opens its tab (kept); the preview stays until replaced.
  await treeRow(page, 'orders').dblclick();
  await expect(stripTabs(page, 0)).toHaveCount(2);
  await expect(page.locator('.tab.preview')).toHaveCount(1);
  await expect(page.locator('.tab:not(.preview)')).toContainText('orders');
});
