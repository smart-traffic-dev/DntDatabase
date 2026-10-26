import type { Locator, Page } from '@playwright/test';
import type { AppError, ConnectionConfig, DdlEditOpen, DdlEditPreview, DdlEditReport } from '../../ui/src/bindings';
import { config, ids } from '../../ui/src/test/fixtures/data';
import { byId, expandRow, treeRow } from '../src/app';
import { createBackend, type FakeBackend } from '../src/backend';
import { installBackend } from '../src/shim';
import { expect, test } from '../src/test';

// The DDL editor (P4c) against the canned objects of the fake backend: the MySQL 8 view
// `v_orders` and procedure `p_restock`, the PostgreSQL 17 function `order_total`. The fake
// echoes the edited text as the statement; real DDL is covered by the Rust tests.

async function connect(page: Page, name: string, over: Partial<ConnectionConfig> = {}): Promise<FakeBackend> {
  const backend = createBackend();
  await backend.handlers.create_connection!({ cfg: config(ids.prod, name, { default_db: 'dnt_fixture', ...over }).config });
  backend.calls.length = 0;
  await installBackend(page, backend.handlers);
  await page.goto('/');
  await expandRow(page, name);
  await expandRow(page, 'dnt_fixture');
  return backend;
}

const editor = (page: Page): Locator => byId(page, 'ddl-editor');
const editorText = (page: Page): Locator => editor(page).locator('.monaco-editor .view-lines');

/** Opens `v_orders` (it opens on Data), its DDL section, then Edit. */
async function editViewFromDdlPanel(page: Page) {
  await expandRow(page, 'Views');
  await treeRow(page, 'v_orders').dblclick();
  await byId(page, 'object-tab-ddl').click();
  await expect(byId(page, 'object-ddl-text')).toContainText('VIEW `v_orders`');
  await page.getByRole('toolbar', { name: 'DDL' }).getByRole('button', { name: 'Edit', exact: true }).click();
  await expect(editor(page)).toContainText('View · v_orders');
  await expect(editorText(page)).toContainText('from `orders`');
}

/** Appends `suffix` at the end of the DDL editor's text. */
async function appendText(page: Page, suffix: string) {
  await editorText(page).click();
  await page.keyboard.press('Control+End');
  await page.keyboard.type(suffix);
  await page.keyboard.press('Escape'); // close a suggestion list
  await expect(editorText(page)).toContainText(suffix.trim());
}

const review = (page: Page) => editor(page).getByRole('button', { name: 'Review & apply…' });
const dialogOf = (page: Page) => page.getByRole('dialog', { name: 'Review & apply' });

test.describe('DDL editor (browser, fake IPC backend)', () => {
  test('view: Edit from the DDL section, review the statement, apply, the tab is clean again', async ({ page }) => {
    const backend = await connect(page, 'shop-staging', { env_tag: 'staging' });
    await editViewFromDdlPanel(page);
    await expect(review(page)).toBeDisabled();

    await appendText(page, ' where `orders`.`total` > 0');
    await review(page).click();
    const dialog = dialogOf(page);
    await expect(dialog).toContainText('where `orders`.`total` > 0');
    await expect(dialog.getByRole('textbox', { name: 'Confirmation name' })).toHaveCount(0);
    await dialog.getByRole('button', { name: 'Apply', exact: true }).click();

    await expect(page.getByText('Changes applied.')).toBeVisible();
    await expect(dialog).toBeHidden();
    // Base is the applied text: nothing to revert, nothing to review.
    await expect(editor(page).getByRole('button', { name: 'Revert' })).toBeDisabled();
    await expect(review(page)).toBeDisabled();
    expect(backend.ddl.objects.get('mysql/View/v_orders')).toContain('where `orders`.`total` > 0');
  });

  test('PROD connection: Apply stays disabled until the exact view name is typed', async ({ page }) => {
    const backend = await connect(page, 'shop-prod', { env_tag: 'production' });
    await editViewFromDdlPanel(page);
    await appendText(page, ' where 1 = 1');

    await review(page).click();
    const dialog = dialogOf(page);
    const apply = dialog.getByRole('button', { name: 'Apply', exact: true });
    const confirm = dialog.getByRole('textbox', { name: 'Confirmation name' });
    await expect(dialog).toContainText('Type v_orders to apply');
    await expect(apply).toBeDisabled();
    await confirm.fill('v_order');
    await expect(apply).toBeDisabled();
    await confirm.fill('v_orders');
    await expect(apply).toBeEnabled();
    await apply.click();

    await expect(page.getByText('Changes applied.')).toBeVisible();
    const sent = backend.calls.find((c) => c.cmd === 'ddl_edit_apply');
    expect(sent?.args).toMatchObject({ confirmed: true, name: 'v_orders' });
  });

  test('stale preview: Reopen keeps the typed text and the next review goes through', async ({ page }) => {
    const backend = await connect(page, 'shop-staging', { env_tag: 'staging' });
    await editViewFromDdlPanel(page);
    await appendText(page, ' where 2 = 2');
    // Another session changes the view after the edit opened.
    backend.ddl.objects.set('mysql/View/v_orders', 'CREATE VIEW `v_orders` AS select 42 AS `answer`');

    await review(page).click();
    const dialog = dialogOf(page);
    await expect(dialog).toContainText('The object changed since you opened it.');
    await dialog.getByRole('button', { name: 'Reopen' }).click();
    await expect(dialog).toBeHidden();

    await expect(editorText(page)).toContainText('where 2 = 2');
    await expect(review(page)).toBeEnabled();
    await review(page).click();
    await expect(dialogOf(page)).toContainText('where 2 = 2');
    const previews = backend.calls.filter((c) => c.cmd === 'ddl_edit_preview');
    expect(previews.at(-1)?.args.text).toContain('where 2 = 2');
    expect(backend.calls.filter((c) => c.cmd === 'ddl_edit_open')).toHaveLength(2);
  });

  test('MySQL procedure partial failure: Put the original definition in the editor', async ({ page }) => {
    const backend = await connect(page, 'shop-staging', { env_tag: 'staging' });
    backend.ddl.partial = true;
    await expandRow(page, 'Procedures');
    await treeRow(page, 'p_restock').click({ button: 'right' });
    await page.getByRole('menuitem', { name: 'Edit definition' }).click();
    await expect(editor(page)).toContainText('Procedure · p_restock');
    await appendText(page, ' -- broken');

    await review(page).click();
    const dialog = dialogOf(page);
    await expect(dialog).toContainText('DROP PROCEDURE `dnt_fixture`.`p_restock`');
    await expect(dialog).toContainText('Not transactional');
    await dialog.getByRole('button', { name: 'Apply', exact: true }).click();

    await expect(dialog).toContainText('The changes were applied only in part.');
    await expect(dialog).toContainText('The object no longer exists on the server.');
    await expect(dialog.getByLabel('Failed statement')).toContainText('-- broken');
    await dialog.getByRole('button', { name: 'Put the original definition in the editor' }).click();
    await expect(dialog).toBeHidden();

    // The edit becomes a create of the original text.
    await expect(editor(page)).toContainText('Procedure · new');
    await expect(editorText(page)).toContainText('UPDATE orders SET total = total + qty');
    await expect(editorText(page)).not.toContainText('-- broken');
  });

  test('PostgreSQL: New function… on the Functions folder opens a template tab', async ({ page }) => {
    await connect(page, 'pg-staging', { kind: 'postgres', port: 5432, env_tag: 'staging' });
    await expandRow(page, 'public');
    await treeRow(page, 'Functions').click({ button: 'right' });
    await page.getByRole('menuitem', { name: 'New function…' }).click();

    await expect(page.getByRole('tab', { name: /New function/ })).toBeVisible();
    await expect(editor(page)).toContainText('Function · new');
    await expect(editorText(page)).toContainText('CREATE FUNCTION');
    // A new object can be reviewed as it is.
    await expect(review(page)).toBeEnabled();
  });

  test('fake fidelity: confirmation, plan expiry, partial apply and database scope as the backend', async () => {
    const backend = createBackend();
    const h = backend.handlers;
    await h.create_connection!({ cfg: config(ids.prod, 'shop-prod', { default_db: 'dnt_fixture', env_tag: 'production' }).config });
    await h.open_connection!({ id: ids.prod });
    const scope = { database: 'dnt_fixture', schema: null };
    // The tree lists the canned objects in `dnt_fixture` only.
    expect(await h.list_objects!({ id: ids.prod, scope, kind: 'View' })).toHaveLength(1);
    expect(await h.list_objects!({ id: ids.prod, scope: { database: 'other', schema: null }, kind: 'View' })).toEqual([]);

    const open = (await h.ddl_edit_open!({ connectionId: ids.prod, scope, kind: 'Procedure', name: 'p_restock', table: null })) as DdlEditOpen;
    const preview = async () => (await h.ddl_edit_preview!({ editId: open.edit_id, text: open.text, strategy: open.strategy })) as DdlEditPreview;
    const errorOf = async (f: () => unknown) => {
      try {
        await f();
      } catch (e) {
        // Handlers throw a `Failure`; the shim hands its `error` to the app.
        return (e as { error: AppError }).error;
      }
      throw new Error('expected an error');
    };
    let p = await preview();
    expect(p.statements[0]).toBe('DROP PROCEDURE `dnt_fixture`.`p_restock`');
    // A wrong typed name is refused and consumes the plan, as `ddl_edit_apply`.
    const refused = await errorOf(() => h.ddl_edit_apply!({ planId: p.plan_id, confirmed: true, name: 'p_restoc', operationId: 'op1' }));
    expect(refused.kind).toBe('confirmation_required');
    expect((await errorOf(() => h.ddl_edit_apply!({ planId: p.plan_id, confirmed: true, name: 'p_restock', operationId: 'op2' }))).kind).toBe('plan_expired');

    // A partial apply turns the edit into a new one: its next preview is a plain create.
    backend.ddl.partial = true;
    p = await preview();
    const r = (await h.ddl_edit_apply!({ planId: p.plan_id, confirmed: true, name: 'p_restock', operationId: 'op3' })) as DdlEditReport;
    expect(r).toMatchObject({ outcome: 'partial', object_state: 'missing' });
    backend.ddl.partial = false;
    p = await preview();
    expect(p.statements).toEqual([open.text]);
  });

  test('closing a dirty DDL tab asks; Cancel keeps the tab and its text', async ({ page }) => {
    await connect(page, 'shop-staging', { env_tag: 'staging' });
    await expandRow(page, 'Views');
    await treeRow(page, 'v_orders').click({ button: 'right' });
    await page.getByRole('menuitem', { name: 'Edit definition' }).click();
    await expect(editor(page)).toContainText('View · v_orders');
    await appendText(page, ' where 3 = 3');

    await page.getByRole('button', { name: 'Close v_orders' }).click();
    const ask = page.getByRole('alertdialog', { name: 'Discard 1 unsaved change?' });
    await expect(ask).toBeVisible();
    await ask.getByRole('button', { name: 'Cancel' }).click();
    await expect(ask).toBeHidden();

    await expect(editor(page)).toBeVisible();
    await expect(editorText(page)).toContainText('where 3 = 3');
  });
});
