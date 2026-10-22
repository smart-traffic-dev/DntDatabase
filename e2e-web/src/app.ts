import { expect, type Page } from '@playwright/test';

export const byId = (page: Page, id: string) => page.getByTestId(id);

/** Tree row whose label is exactly `label` (rows for loading / error placeholders have no label). */
export const treeRow = (page: Page, label: string) =>
  page.getByTestId('tree-row').filter({ has: page.locator('span.label').getByText(label, { exact: true }) });

/** Expands a collapsed tree row (a group, a connection, a database / schema, a folder). */
export async function expandRow(page: Page, label: string): Promise<void> {
  const row = treeRow(page, label);
  await expect(row).toBeVisible();
  if ((await row.getAttribute('aria-expanded')) !== 'true') {
    await row.locator('span.chev').click();
  }
  await expect(row).toHaveAttribute('aria-expanded', 'true');
}

export interface ConnectionForm {
  name: string;
  engine: 'mysql' | 'postgres';
  host?: string;
  port: number;
  user: string;
  password: string;
}

/** Fills the New connection dialog (one page: name, engine, server, login). */
export async function fillConnectionDialog(page: Page, form: ConnectionForm): Promise<void> {
  await byId(page, 'new-connection').click();
  await byId(page, `conn-engine-${form.engine}`).check();
  await byId(page, 'conn-host').fill(form.host ?? '127.0.0.1');
  await byId(page, 'conn-port').fill(String(form.port));
  await byId(page, 'conn-user').fill(form.user);
  await byId(page, 'conn-password').fill(form.password);
  await byId(page, 'conn-name').fill(form.name);
}

/** Saves the open connection dialog; the tree reloads with the new connection. */
export async function saveConnectionDialog(page: Page): Promise<void> {
  await byId(page, 'conn-save').click();
  await expect(byId(page, 'conn-name')).toBeHidden();
}

/**
 * Double-clicks an object in the tree (a table opens on its Data sub-tab), switches to its
 * DDL sub-tab and checks the DDL (shown in a read-only Monaco editor).
 */
export async function openDdl(page: Page, objectName: string): Promise<void> {
  const row = treeRow(page, objectName);
  await expect(row).toBeVisible();
  await row.dblclick();

  const ddlTab = byId(page, 'object-tab-ddl');
  await expect(ddlTab).toBeVisible();
  await ddlTab.click();

  await expect(byId(page, 'object-ddl-text')).toContainText('CREATE TABLE');
}
