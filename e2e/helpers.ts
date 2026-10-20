import { $, browser } from '@wdio/globals';

export const byId = (id: string) => $(`[data-testid="${id}"]`);

/** Tree row whose label is exactly `label`. */
export const treeRow = (label: string) =>
  $(`//*[@data-testid="tree-row"][.//span[contains(@class,"label") and normalize-space(.)="${label}"]]`);

export async function expandRow(label: string) {
  const row = await treeRow(label);
  await row.waitForDisplayed({ timeout: 20_000 });
  if ((await row.getAttribute('aria-expanded')) !== 'true') {
    await row.doubleClick();
  }
  await browser.waitUntil(async () => (await row.getAttribute('aria-expanded')) === 'true', {
    timeout: 20_000,
    timeoutMsg: `"${label}" did not expand`,
  });
}

/** Collapses the row so same-named children of another connection are unambiguous. */
export async function collapseRow(label: string) {
  const row = await treeRow(label);
  if ((await row.getAttribute('aria-expanded')) === 'true') await row.doubleClick();
  await browser.waitUntil(async () => (await row.getAttribute('aria-expanded')) !== 'true', {
    timeout: 20_000,
    timeoutMsg: `"${label}" did not collapse`,
  });
}

export interface ConnOpts {
  name: string;
  engine: 'mysql' | 'mariadb' | 'postgres';
  port: number;
  user: string;
  password: string;
  env?: 'production' | 'staging' | 'local';
}

/** Fills the connection dialog, tests, and saves. */
export async function createConnection(o: ConnOpts) {
  await byId('new-connection').waitForClickable({ timeout: 30_000 });
  await byId('new-connection').click();
  await byId('conn-name').setValue(o.name);
  await byId(`conn-engine-${o.engine}`).click();
  await byId('conn-host').setValue('127.0.0.1');
  await byId('conn-port').setValue(String(o.port));
  await byId('conn-user').setValue(o.user);
  await byId('conn-password').setValue(o.password);
  if (o.env) await $('#cd-env').selectByAttribute('value', o.env);
  await byId('conn-save').click();
}

export async function focusEditor() {
  const editor = await $('[data-testid="query-editor"] .monaco-editor');
  await editor.waitForDisplayed({ timeout: 30_000 });
  await editor.click();
}

/** Replaces the editor content with `sql` (single line). */
export async function typeInEditor(sql: string) {
  await focusEditor();
  await browser.keys(['Control', 'a']);
  await browser.keys(['Delete']);
  await browser.keys(sql.split(''));
  // Dismiss any suggestion widget so Ctrl+Enter reaches the run command.
  await browser.keys(['Escape']);
}

export async function runAll() {
  await focusEditor();
  await browser.keys(['Control', 'Shift', 'Enter']);
}

export async function runCurrent() {
  await focusEditor();
  await browser.keys(['Control', 'Enter']);
}
