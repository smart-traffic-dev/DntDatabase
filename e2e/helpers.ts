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

/** MariaDB shares the MySQL driver: one engine choice covers both. */
export const engineChoice = (engine: 'mysql' | 'mariadb' | 'postgres') => (engine === 'postgres' ? 'postgres' : 'mysql');

/** Fills the connection dialog (one page) and saves. */
export async function createConnection(o: ConnOpts) {
  await byId('new-connection').waitForClickable({ timeout: 30_000 });
  await byId('new-connection').click();
  await byId(`conn-engine-${engineChoice(o.engine)}`).click();
  await byId('conn-host').setValue('127.0.0.1');
  await byId('conn-port').setValue(String(o.port));
  await byId('conn-user').setValue(o.user);
  await byId('conn-password').setValue(o.password);
  await byId('conn-name').setValue(o.name);
  if (o.env) await byId(`conn-env-${o.env}`).click();
  await byId('conn-save').click(); // Save
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

/** The query toolbar's Run slot shows Stop while a statement (or an export) can be stopped. */
export const isRunning = () => byId('query-stop').isExisting();

/**
 * Opens the context pill's picker, runs `fn` (it holds the `query-connection` and
 * `query-database` selects), then closes it with Escape.
 */
export async function inContextPicker(fn: () => Promise<void>) {
  await byId('query-context').click();
  await byId('query-connection').waitForDisplayed({ timeout: 5_000 });
  await fn();
  await browser.keys(['Escape']);
  await byId('query-connection').waitForDisplayed({ reverse: true, timeout: 5_000 });
}

/**
 * Sets the row limit of the active query tab: a radio item of the `⋯` menu at the right end of
 * the group's tab strip (choosing it closes the menu).
 */
export async function setRowLimit(limit: 1000 | 10000 | 100000 | 1000000) {
  await byId('query-more').click();
  await byId(`query-row-limit-${limit}`).waitForClickable({ timeout: 5_000 });
  await byId(`query-row-limit-${limit}`).click();
}

/** Toggles "Continue on error" in the Run split menu (choosing it closes the menu). */
export async function toggleContinueOnError() {
  await byId('query-run-menu').click();
  await byId('query-continue-on-error').waitForClickable({ timeout: 5_000 });
  await byId('query-continue-on-error').click();
}
