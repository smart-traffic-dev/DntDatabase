import { $, $$, browser, expect } from '@wdio/globals';
import { evidence } from './evidence';
import { byId, collapseRow, createConnection, expandRow, inContextPicker, isRunning, runAll, runCurrent, treeRow, typeInEditor, type ConnOpts } from './helpers';

interface Engine {
  prefix: string;
  conn: ConnOpts;
  sleep: string;
  /** Tree path below the connection down to the node whose click selects `dnt_fixture`. */
  expandPath: string[];
}

const engines: Engine[] = [
  {
    prefix: 'q-my',
    conn: { name: 'e2e-q-mysql8', engine: 'mysql', port: 33080, user: 'root', password: 'dnt_test', env: 'production' },
    sleep: 'SELECT SLEEP(30)',
    expandPath: ['e2e-q-mysql8'],
  },
  {
    prefix: 'q-pg',
    conn: { name: 'e2e-q-pg17', engine: 'postgres', port: 54317, user: 'postgres', password: 'dnt_test', env: 'production' },
    sleep: 'SELECT pg_sleep(30)',
    expandPath: ['e2e-q-pg17'],
  },
];

const summaryText = async () => {
  const el = await byId('result-summary');
  await el.waitForDisplayed({ timeout: 30_000 });
  return el.getText();
};

const messageLines = async () => $$('[data-testid="message-line"]');

async function confirmRun() {
  const dlg = await byId('run-confirm');
  await dlg.waitForDisplayed({ timeout: 15_000 });
  return dlg;
}

for (const e of engines) {
  describe(`DntDatabase query editor (${e.conn.name})`, () => {
    it('runs, confirms and stops queries on a Production connection', async () => {
      await createConnection(e.conn);
      for (const label of e.expandPath) await expandRow(label);
      await expandRow('dnt_fixture');
      await (await treeRow('dnt_fixture')).click();

      // 1. New query tab bound to dnt_fixture.
      await byId('new-query').click();
      await (await $('[data-testid="query-editor"] .monaco-editor')).waitForDisplayed({ timeout: 30_000 });
      await inContextPicker(async () => {
        await expect(byId('query-database')).toHaveValue('dnt_fixture');
        await expect($('[data-testid="query-connection"] option:checked')).toHaveText(expect.stringContaining(e.conn.name));
      });
      await evidence(`${e.prefix}-01-new-query`);

      // No request goes to a CDN: Monaco is bundled.
      const cdn = await browser.execute(() =>
        performance
          .getEntriesByType('resource')
          .map((r) => r.name)
          .filter((n) => n.includes('cdn.jsdelivr')),
      );
      expect(cdn).toEqual([]);

      // 2. SELECT -> result grid summary.
      await typeInEditor('SELECT id, status FROM orders');
      await runCurrent();
      await browser.waitUntil(async () => /\d+ rows/.test(await summaryText().catch(() => '')), {
        timeout: 30_000,
        timeoutMsg: 'no "N rows" summary after SELECT',
      });
      const rows = Number((await summaryText()).match(/(\d+) rows/)![1]);
      expect(rows).toBeGreaterThan(0);
      await expect(byId('result-grid')).toBeDisplayed();
      await evidence(`${e.prefix}-02-select-grid`);

      // 3. Script with an error: Messages show it and the editor carries a marker.
      await typeInEditor('SELECT 1; SELECT * FORM orders');
      await runAll();
      // Production may ask first; either the dialog or the error line shows up.
      await browser.waitUntil(
        async () => {
          if (await byId('run-confirm').isDisplayed()) return true;
          if (await byId('panel-messages').isDisplayed()) await byId('panel-messages').click();
          return (await $$('[data-testid="message-line"].error')).length > 0;
        },
        { timeout: 30_000, timeoutMsg: 'script neither asked nor failed' },
      );
      if (await byId('run-confirm').isDisplayed()) {
        await evidence(`${e.prefix}-03a-confirm-script`);
        await byId('run-confirm-run').click();
        await byId('run-confirm').waitForDisplayed({ reverse: true, timeout: 15_000 });
      }
      await byId('panel-messages').click();
      await browser.waitUntil(async () => (await $$('[data-testid="message-line"].error')).length > 0, {
        timeout: 30_000,
        timeoutMsg: 'no error message line',
      });
      await browser.waitUntil(async () => (await $$('.monaco-editor .squiggly-error')).length > 0, {
        timeout: 10_000,
        timeoutMsg: 'no error marker in the editor',
      });
      await evidence(`${e.prefix}-03-error-script`);

      // 4. Setup script on Production asks for confirmation.
      await typeInEditor('CREATE TABLE e2e_guard (id INT); INSERT INTO e2e_guard VALUES (1), (2)');
      await runAll();
      await confirmRun();
      await evidence(`${e.prefix}-04-confirm-setup`);
      await byId('run-confirm-run').click();
      await byId('run-confirm').waitForDisplayed({ reverse: true, timeout: 15_000 });
      await byId('panel-messages').click();
      await browser.waitUntil(async () => (await $$('[data-testid="message-line"]')).length >= 3, {
        timeout: 30_000,
        timeoutMsg: 'setup script did not finish',
      });
      expect((await $$('[data-testid="message-line"].error')).length).toBe(0);
      await evidence(`${e.prefix}-05-setup-done`);

      // 5. DELETE asks; Cancel runs nothing.
      const before = (await messageLines()).length;
      await typeInEditor('DELETE FROM e2e_guard');
      await runCurrent();
      const dlg = await confirmRun();
      await expect(dlg).toHaveText(expect.stringContaining('WRITE'));
      await evidence(`${e.prefix}-06-confirm-delete`);
      await byId('run-confirm-cancel').click();
      await byId('run-confirm').waitForDisplayed({ reverse: true, timeout: 15_000 });
      await browser.pause(500);
      expect((await messageLines()).length).toBe(before);

      // 6. Nothing was deleted.
      await typeInEditor('SELECT * FROM e2e_guard');
      await runCurrent();
      await browser.waitUntil(async () => (await summaryText().catch(() => '')).startsWith('2 rows'), {
        timeout: 30_000,
        timeoutMsg: 'e2e_guard does not hold 2 rows',
      });
      await evidence(`${e.prefix}-07-nothing-deleted`);

      // 7. Explain.
      await typeInEditor('SELECT id, status FROM orders');
      await byId('query-explain').click();
      await byId('explain-view').waitForDisplayed({ timeout: 30_000 });
      await evidence(`${e.prefix}-08-explain`);

      // 8. Stop a sleeping query.
      await typeInEditor(e.sleep);
      await runCurrent();
      await browser.waitUntil(async () => await isRunning(), { timeout: 15_000 });
      await evidence(`${e.prefix}-09-sleeping`);
      const t0 = Date.now();
      await byId('query-stop').click();
      await browser.waitUntil(async () => !(await isRunning()), {
        timeout: 20_000,
        timeoutMsg: 'run did not stop',
      });
      console.log(`${e.prefix}: stop took ${Date.now() - t0} ms`);
      await byId('panel-messages').click();
      await evidence(`${e.prefix}-10-stopped`);

      // 9. Cleanup.
      await typeInEditor('DROP TABLE e2e_guard');
      await runCurrent();
      await confirmRun();
      // P3: DROP on a Production connection needs its name typed before Run enables.
      await expect(byId('run-confirm-run')).toBeDisabled();
      await byId('run-confirm-name').setValue('e2e_guard');
      await byId('run-confirm-run').click();
      await byId('run-confirm').waitForDisplayed({ reverse: true, timeout: 15_000 });
      await browser.waitUntil(async () => (await $$('[data-testid="message-line"]')).length > 0, { timeout: 30_000 });
      await evidence(`${e.prefix}-11-cleanup`);
      // Both engines have a dnt_fixture: fold this connection so the next run picks its own.
      await collapseRow(e.conn.name);
    });
  });
}
