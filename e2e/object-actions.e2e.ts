import { execFileSync } from 'node:child_process';

import { randomUUID } from 'node:crypto';

import { $, browser, expect } from '@wdio/globals';

import { evidence } from './evidence';
import { byId, collapseRow, createConnection, expandRow, runAll, treeRow, typeInEditor } from './helpers';



async function menu(label: string, action: string) {

  const row = await treeRow(label);

  await row.waitForDisplayed({ timeout: 20_000 });

  await row.click({ button: 'right' });

  const item = await $(`//*[@role='menuitem'][.//span[normalize-space(.)='${action}']]`);

  await item.waitForClickable({ timeout: 10_000 });

  await item.click();

}

/** The dialog shows its SQL on open (drop / truncate) or as the name is typed (rename); a clean apply closes it. */
async function apply(verb: string, typed?: string) {

  await $('[aria-label="SQL preview"]').waitUntil(async function () {
    return /^(DROP|TRUNCATE|RENAME|ALTER)/m.test(await this.getText());
  }, { timeout: 20_000 });

  if (typed) await $('[aria-label="Confirmation name"]').setValue(typed);
  await evidence(`p4a-preview-${typed ?? 'ordinary'}`);

  await $(`button=${verb}`).click();

  await $('.object-action-dialog').waitForDisplayed({ reverse: true, timeout: 30_000 });

}

for (const pg of [false, true]) {

  describe(`P4a object actions (${pg ? 'PostgreSQL 17' : 'MySQL 8'})`, () => {

    const ns = `p4a_smoke_${randomUUID().replaceAll('-', '').slice(0, 12)}`;

    const conn = `p4a-${pg ? 'pg' : 'my'}`;

    after(() => {

      if (!/^p4a_smoke_[0-9a-f]{12}$/.test(ns)) throw new Error('Invalid cleanup namespace');

      const args = pg

        ? ['exec', 'dnt-test-pg17-1', 'psql', '-U', 'postgres', '-d', 'dnt_fixture', '-c', `DROP SCHEMA IF EXISTS ${ns} CASCADE`]

        : ['exec', 'dnt-test-mysql8-1', 'mysql', '-uroot', '-pdnt_test', '-e', `DROP DATABASE IF EXISTS ${ns}`];

      execFileSync('docker', args, { stdio: 'ignore' });

    });

    it('creates a namespace, previews and applies rename/truncate/drop through the real IPC', async () => {

      await browser.keys(['Escape']);
      for (const other of ['p4a-my', 'p4a-pg']) {
        if (await (await treeRow(other)).isExisting()) await collapseRow(other);
      }
      await createConnection({ name: conn, engine: pg ? 'postgres' : 'mysql', port: pg ? 54317 : 33080, user: pg ? 'postgres' : 'root', password: 'dnt_test', env: 'local' });

      await expandRow(conn);

      if (pg) await expandRow('dnt_fixture');

      await menu(pg ? 'dnt_fixture' : conn, pg ? 'Create schema…' : 'Create database…');

      await $('[aria-label="Name"]').setValue(ns);

      await $('[aria-label="SQL preview"]').waitUntil(async function () { return (await this.getText()).includes(ns); }, { timeout: 20_000 });
      await evidence('p4a-create-namespace');
      await $(`button=${pg ? 'Create schema' : 'Create database'}`).click();
      await $('.create-ns-dialog').waitForDisplayed({ reverse: true, timeout: 30_000 });

      await expandRow(conn);

      if (pg) await expandRow('dnt_fixture');

      await expandRow(ns);

      await (await treeRow(ns)).click();

      await byId('new-query').click();

      const rel = pg ? `"${ns}"."items"` : "`" + ns + "`.`items`";

      await typeInEditor(`CREATE TABLE ${rel} (id INT PRIMARY KEY); INSERT INTO ${rel} VALUES (1)`);

      await runAll();

      await byId('panel-messages').waitForClickable({ timeout: 20_000 });
      await byId('panel-messages').click();
      await browser.waitUntil(async () => (await byId('messages').getText()).includes('Done: 2 statements'), { timeout: 20_000 });
      await expect(byId('messages')).not.toHaveText(expect.stringContaining('Error:'));
      // SQL entered manually uses the tree's explicit Refresh action. P4a actions refresh automatically.
      await (await treeRow(ns)).click();
      await browser.keys(['F5']);

      await expandRow('Tables');

      await menu('items', 'Rename table…');

      await $('[aria-label="Name"]').setValue('renamed');

      await apply('Rename table');

      await expandRow(conn);

      if (pg) await expandRow('dnt_fixture');

      await expandRow(ns); await expandRow('Tables');

      await menu('renamed', 'Truncate table…');

      await apply('Truncate table', 'renamed');

      await expandRow(conn);

      if (pg) await expandRow('dnt_fixture');

      await expandRow(ns); await expandRow('Tables');

      await menu('renamed', 'Drop table…');

      await apply('Drop table', 'renamed');

      await expandRow(conn);

      if (pg) await expandRow('dnt_fixture');

      await menu(ns, pg ? 'Drop schema…' : 'Drop database…');

      await apply(pg ? 'Drop schema' : 'Drop database', ns);

      await expect($('[aria-label="SQL preview"]')).not.toExist();

      // Collapse this connection to keep fixture names unambiguous in the next engine.

      const row = await treeRow(conn);

      if (await row.getAttribute('aria-expanded') === 'true') await row.doubleClick();

    });

  });

}

