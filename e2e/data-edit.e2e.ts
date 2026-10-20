import { randomUUID } from 'node:crypto';
import { $, $$, browser, expect } from '@wdio/globals';
import { evidence } from './evidence';
import { byId, collapseRow, createConnection, expandRow, runAll, runCurrent, treeRow, typeInEditor, type ConnOpts } from './helpers';

interface Engine {
  prefix: string;
  conn: ConnOpts;
  /** Tree rows to expand (below the connection) to reach the table list. */
  schemaPath: string[];
}

// Local environment: no Production confirmations get in the way of the scratch statements.
const engines: Engine[] = [
  {
    prefix: 'de-my',
    conn: { name: 'e2e-d-mysql8', engine: 'mysql', port: 33080, user: 'root', password: 'dnt_test', env: 'local' },
    schemaPath: ['dnt_fixture'],
  },
  {
    prefix: 'de-pg',
    conn: { name: 'e2e-d-pg17', engine: 'postgres', port: 54317, user: 'postgres', password: 'dnt_test', env: 'local' },
    schemaPath: ['dnt_fixture', 'public'],
  },
];

const OTHER_CONNECTIONS = ['e2e-mysql8', 'e2e-pg17', 'e2e-q-mysql8', 'e2e-q-pg17', 'e2e-d-mysql8', 'e2e-d-pg17'];

const summaryText = async () => {
  const el = await byId('result-summary');
  await el.waitForDisplayed({ timeout: 30_000 });
  return el.getText();
};

async function newQueryOnFixture() {
  await (await treeRow('dnt_fixture')).click();
  await byId('new-query').click();
  await (await $('[data-testid="query-editor"] .monaco-editor')).waitForDisplayed({ timeout: 30_000 });
}

/** Runs one script and waits until its messages show no error. */
async function runScript(sql: string) {
  await typeInEditor(sql);
  await runAll();
  await byId('panel-messages').waitForDisplayed({ timeout: 30_000 });
  await byId('panel-messages').click();
  await browser.waitUntil(async () => (await $$('[data-testid="message-line"]')).length >= 1, {
    timeout: 30_000,
    timeoutMsg: `script did not finish: ${sql}`,
  });
  expect((await $$('[data-testid="message-line"].error')).length).toBe(0);
}

async function expectRows(sql: string, count: number) {
  await typeInEditor(sql);
  await runCurrent();
  await browser.waitUntil(async () => (await summaryText().catch(() => '')).startsWith(`${count} row`), {
    timeout: 30_000,
    timeoutMsg: `expected ${count} row(s) for: ${sql}`,
  });
}

for (const e of engines) {
  describe(`DntDatabase data editing (${e.conn.name})`, () => {
    const table = `p3_smoke_${randomUUID().replace(/-/g, '').slice(0, 8)}`;

    it('edits a cell in the Data tab, applies it, reads it back and drops the scratch table', async () => {
      // Earlier specs left their connections in the tree: collapse them so `dnt_fixture` is unambiguous.
      for (const name of OTHER_CONNECTIONS) {
        if (name === e.conn.name) continue;
        try {
          if (await (await treeRow(name)).isExisting()) await collapseRow(name);
        } catch {
          // not in the tree
        }
      }
      await createConnection(e.conn);
      await expandRow(e.conn.name);
      for (const label of e.schemaPath) await expandRow(label);

      // 1. Scratch table with a primary key, created from a query tab.
      await newQueryOnFixture();
      await runScript(`CREATE TABLE ${table} (id INT PRIMARY KEY, label VARCHAR(40)); INSERT INTO ${table} VALUES (1, 'before')`);
      await evidence(`${e.prefix}-01-scratch-created`);

      // 2. Open its Data tab (a table opens on Data).
      await expandRow('Tables');
      const row = await treeRow(table);
      await row.waitForDisplayed({ timeout: 20_000 });
      await row.doubleClick();
      const grid = await byId('result-grid');
      await grid.waitForDisplayed({ timeout: 30_000 });
      await $('[role="toolbar"][aria-label="Edit"]').waitForDisplayed({ timeout: 30_000 }); // editable: the edit bar exists
      await evidence(`${e.prefix}-02-data-tab`);

      // 3. Edit `label` of the first row: click the first cell, move right, Enter, type, Enter.
      const canvas = await grid.$('canvas');
      // wdio click offsets are measured from the element's centre; the `label` cell of row 1 sits 120 px
      // right of and 58 px below the canvas's top-left corner (row marker, `id` column, then `label`).
      const size = await canvas.getSize();
      await canvas.click({ x: Math.round(120 - size.width / 2), y: Math.round(58 - size.height / 2) });
      await browser.pause(300);
      // Shift+Enter opens the DOM value editor on the selected cell (the canvas cell overlay is not driveable).
      await browser.keys(['Shift', 'Enter']);
      const box = await $('[role="textbox"][aria-label="Value"], textarea[aria-label="Value"]');
      await box.waitForDisplayed({ timeout: 10_000 });
      await evidence(`${e.prefix}-02b-value-editor`);
      await box.setValue('after');
      await $('button=Save').click();
      const apply = await $('button=Apply changes');
      await apply.waitForDisplayed({ timeout: 10_000 });
      await evidence(`${e.prefix}-03-staged`);

      // 4. Review shows the real statement, then Confirm applies it.
      await apply.click();
      const dialog = await $('[role="dialog"]');
      await dialog.waitForDisplayed({ timeout: 15_000 });
      await expect(dialog).toHaveText(expect.stringContaining(table));
      await expect(dialog).toHaveText(expect.stringContaining('UPDATE'));
      await evidence(`${e.prefix}-04-review`);
      await $('//*[@role="dialog"]//button[normalize-space(.)="Confirm"]').click();
      await dialog.waitForDisplayed({ reverse: true, timeout: 30_000 });
      await apply.waitForDisplayed({ reverse: true, timeout: 15_000 });
      await evidence(`${e.prefix}-05-applied`);

      // 5. Read it back from a query tab.
      await newQueryOnFixture();
      await expectRows(`SELECT id FROM ${table} WHERE label = 'after'`, 1);
      await evidence(`${e.prefix}-06-read-back`);

      // 6. Drop the scratch table.
      await runScript(`DROP TABLE ${table}`);
      await evidence(`${e.prefix}-07-dropped`);
    });
  });
}
