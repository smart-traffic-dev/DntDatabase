// UX audit "screenshot tour": walks the main states of the app and screenshots each one.
// Not a test of behaviour: every step is soft (a failing step is screenshotted as FAILED and
// the tour goes on). Run once per theme and size:
//   AUDIT_THEME=light|dark  AUDIT_SIZE=default|narrow  wdio run ./wdio.conf.ts --spec ./audit.e2e.ts
import fs from 'node:fs';
import path from 'node:path';
import { $, $$, browser } from '@wdio/globals';
import { evidence, evidenceDir } from './evidence';
import { byId, expandRow, inContextPicker, isRunning, setRowLimit, toggleContinueOnError, treeRow } from './helpers';

const THEME = process.env.AUDIT_THEME ?? 'light';
const SIZE = process.env.AUDIT_SIZE ?? 'default';
const P = `${THEME}-${SIZE}`;
let n = 0;
const notes: string[] = [];

const shot = async (name: string) => {
  n += 1;
  if (SIZE === 'narrow') {
    const w = await browser.execute(() => window.innerWidth);
    if (w > 1100) {
      note(`window grew back to ${w}px before "${name}"; resizing again`);
      await browser.setWindowSize(1024, 700);
      await browser.pause(500);
    }
  }
  await browser.pause(250);
  await evidence(`${P}-${String(n).padStart(2, '0')}-${name}`);
};
const note = (s: string) => {
  notes.push(s);
  console.log(`[audit ${P}] ${s}`);
};

async function step(name: string, fn: () => Promise<void>) {
  try {
    await fn();
  } catch (e) {
    note(`STEP FAILED ${name}: ${(e as Error).message.split('\n')[0]}`);
    try {
      await evidence(`${P}-FAILED-${name}`);
    } catch {
      /* ignore */
    }
    // Leave dialogs / menus behind.
    await browser.keys(['Escape']).catch(() => {});
    await browser.keys(['Escape']).catch(() => {});
  }
}

async function waitGone(id: string, timeout = 15_000) {
  await byId(id).waitForDisplayed({ reverse: true, timeout });
}

async function openNewConnection() {
  await byId('new-connection').waitForClickable({ timeout: 30_000 });
  await byId('new-connection').click();
  await byId('conn-name').waitForDisplayed({ timeout: 10_000 });
}

interface Conn {
  name: string;
  engine: 'mysql' | 'mariadb' | 'postgres';
  host?: string;
  port: number;
  user: string;
  password?: string;
  env?: '' | 'production' | 'staging' | 'local';
  readOnly?: boolean;
  savePassword?: boolean;
  group?: string;
}

async function fillGeneral(c: Conn) {
  await byId('conn-name').setValue(c.name);
  await byId(`conn-engine-${c.engine}`).click();
  await byId('conn-host').setValue(c.host ?? '127.0.0.1');
  await byId('conn-port').setValue(String(c.port));
  await byId('conn-user').setValue(c.user);
  if (c.password !== undefined) await byId('conn-password').setValue(c.password);
  if (c.env) await byId(`conn-env-${c.env}`).click();
  if (c.group) await $('#cd-group').selectByVisibleText(c.group);
  if (c.readOnly) await $("//label[contains(.,'Read-only connection')]/input").click();
  if (c.savePassword === false) await $("//label[contains(.,'Save password')]/input").click();
}

async function createConnection(c: Conn) {
  await openNewConnection();
  await fillGeneral(c);
  await byId('conn-save').click();
  await byId('conn-name').waitForDisplayed({ reverse: true, timeout: 15_000 });
}

const editor = () => $('[data-testid="query-editor"] .monaco-editor');

async function focusEditor() {
  const ed = await editor();
  await ed.waitForDisplayed({ timeout: 30_000 });
  await ed.click();
}

/** Replaces the editor text with `sql` through a synthetic paste (fast; multi-line safe). */
async function setSql(sql: string) {
  await focusEditor();
  await browser.keys(['Control', 'a']);
  await browser.keys(['Delete']);
  await browser.execute((text: string) => {
    const ta = document.activeElement as HTMLElement | null;
    const dt = new DataTransfer();
    dt.setData('text/plain', text);
    ta?.dispatchEvent(new ClipboardEvent('paste', { clipboardData: dt, bubbles: true, cancelable: true }));
  }, sql);
  await browser.pause(150);
  await browser.keys(['Escape']);
  const seen = await browser.execute(
    () => (document.querySelector('[data-testid="query-editor"] .view-lines') as HTMLElement | null)?.innerText ?? '',
  );
  const norm = (s: string) => s.replace(/\s+/g, '');
  if (sql !== '' && (norm(seen) === '' || !norm(sql).includes(norm(seen)))) {
    note(`paste fallback for: ${sql.slice(0, 40)}`);
    await focusEditor();
    await browser.keys(['Control', 'a']);
    await browser.keys(['Delete']);
    await browser.keys(sql.split(''));
    await browser.keys(['Escape']);
  }
}

async function runAll() {
  await focusEditor();
  await browser.keys(['Control', 'Shift', 'Enter']);
}

async function runCurrent() {
  await focusEditor();
  await browser.keys(['Control', 'Enter']);
}

const idle = async () => {
  await browser.pause(800);
  return browser.waitUntil(async () => !(await isRunning()), { timeout: 60_000, timeoutMsg: 'run did not end' });
};

async function confirmIfAsked(screenshot?: string) {
  await browser.pause(400);
  if (await byId('run-confirm').isDisplayed().catch(() => false)) {
    if (screenshot) await shot(screenshot);
    await byId('run-confirm-run').click();
    await waitGone('run-confirm');
  }
}

async function rightClick(label: string) {
  const row = await treeRow(label);
  await row.waitForDisplayed({ timeout: 20_000 });
  await row.click({ button: 'right' });
  await $('[role="menu"]').waitForDisplayed({ timeout: 5_000 });
}

async function newQueryOn(label: string) {
  await (await treeRow(label)).click();
  await byId('new-query').click();
  await (await editor()).waitForDisplayed({ timeout: 30_000 });
}

/** Records the focus order of `presses` Tab presses from the current focus. */
async function tabOrder(presses: number): Promise<string[]> {
  const out: string[] = [];
  for (let i = 0; i < presses; i++) {
    await browser.keys(['Tab']);
    out.push(
      await browser.execute(() => {
        const el = document.activeElement as HTMLElement | null;
        if (!el) return '(none)';
        const label =
          el.getAttribute('aria-label') ||
          (el.id && document.querySelector(`label[for="${el.id}"]`)?.textContent) ||
          el.textContent?.trim().slice(0, 30) ||
          (el as HTMLInputElement).value ||
          '';
        return `${el.tagName.toLowerCase()}${el.id ? '#' + el.id : ''}[${label}]`;
      }),
    );
  }
  return out;
}

describe(`UX audit tour (${P})`, () => {
  it('walks every main state', async function () {
    this.timeout(30 * 60_000);
    // ---- theme and size ----
    await byId('new-connection').waitForClickable({ timeout: 30_000 });
    await browser.execute((t: string) => localStorage.setItem('theme', t), THEME);
    await browser.refresh();
    await byId('new-connection').waitForClickable({ timeout: 30_000 });
    if (SIZE === 'narrow') {
      try {
        await browser.setWindowSize(1024, 700);
      } catch (e) {
        note(`setWindowSize failed: ${(e as Error).message.split('\n')[0]}`);
      }
      await browser.pause(500);
    }
    note(`viewport ${JSON.stringify(await browser.execute(() => [window.innerWidth, window.innerHeight, devicePixelRatio]))}`);

    await step('empty-start', async () => {
      await shot('empty-start');
    });

    await step('settings', async () => {
      await $('[aria-label="Connection menu"]').click();
      await $('[role="menu"]').waitForDisplayed({ timeout: 5_000 });
      await shot('connection-menu');
      await $("//button[@role='menuitem'][contains(.,'Settings')]").click();
      await $("//h2[.='Settings']").waitForDisplayed({ timeout: 5_000 });
      await shot('settings-dialog');
      await browser.keys(['Escape']);
    });

    // ---- connection dialog ----
    await step('conn-dialog', async () => {
      await openNewConnection();
      await shot('conn-general-empty');
      if (THEME === 'light' && SIZE === 'default') {
        note(`conn dialog tab order: ${(await tabOrder(22)).join(' > ')}`);
      }
      await byId('conn-save').click();
      await browser.pause(800);
      await shot('conn-save-empty-field-error');
      await fillGeneral({ name: 'shop-prod', engine: 'mysql', port: 33080, user: 'root', password: 'dnt_test', env: 'production' });
      await shot('conn-general-prod');
      for (const t of ['ssh', 'ssl', 'advanced']) {
        await $(`#cd-tab-${t}`).click();
        await shot(`conn-tab-${t}`);
      }
      await $('#cd-tab-general').click();
      await byId('conn-test').click();
      await byId('test-result').waitForDisplayed({ timeout: 30_000 });
      await shot('conn-test-ok');
      await byId('conn-port').setValue('33999');
      await byId('conn-test').click();
      await browser.waitUntil(async () => (await byId('test-result').getText()).length > 0 && !(await byId('test-result').getText()).startsWith('Connected'), { timeout: 30_000 });
      await shot('conn-test-fail');
      const details = await $("//div[@data-testid='test-result']//button[.='Details']");
      if (await details.isExisting()) {
        await details.click();
        await shot('conn-test-fail-details');
      }
      // Bad host name: DNS failure stage
      await byId('conn-host').setValue('no-such-host.invalid');
      await byId('conn-port').setValue('33080');
      await byId('conn-test').click();
      await browser.waitUntil(async () => !(await byId('test-result').getText()).startsWith('Connected'), { timeout: 30_000 });
      await browser.pause(3000);
      await shot('conn-test-fail-dns');
      await byId('conn-host').setValue('127.0.0.1');
      // Wrong password
      await byId('conn-password').setValue('wrong');
      await byId('conn-test').click();
      await browser.pause(3000);
      await shot('conn-test-fail-auth');
      await byId('conn-password').setValue('dnt_test');
      // Focus ring on a button
      await byId('conn-test').click();
      await browser.pause(1500);
      await browser.keys(['Tab']);
      await shot('conn-focus-after-test');
      await byId('conn-save').click();
      await byId('conn-name').waitForDisplayed({ reverse: true, timeout: 15_000 });
    });

    // ---- group + more connections ----
    await step('group', async () => {
      await rightClick('shop-prod');
      await shot('ctx-menu-connection-closed');
      await $("//button[@role='menuitem'][contains(.,'New group')]").click();
      await $('#nd-name').setValue('Analytics');
      await browser.keys(['Enter']);
      await browser.pause(800);
    });
    await step('more-connections', async () => {
      await createConnection({ name: 'analytics-pg', engine: 'postgres', port: 54317, user: 'postgres', password: 'dnt_test', env: 'local', group: 'Analytics' });
      await createConnection({ name: 'reporting-ro', engine: 'mysql', port: 33080, user: 'root', password: 'dnt_test', env: 'staging', readOnly: true, group: 'Analytics' });
      await createConnection({ name: 'ask-pw', engine: 'postgres', port: 54317, user: 'postgres', savePassword: false });
      await createConnection({ name: 'dead-server', engine: 'mysql', port: 33999, user: 'root', password: 'x' });
      await createConnection({
        name: 'a-very-long-connection-name-for-the-legacy-customer-relationship-system',
        engine: 'mariadb',
        port: 33114,
        user: 'root',
        password: 'dnt_test',
      });
      await shot('tree-with-groups');
    });

    // ---- SSH host key ----
    await step('ssh-hostkey', async () => {
      await openNewConnection();
      await fillGeneral({ name: 'via-ssh', engine: 'mysql', host: 'mysql8', port: 3306, user: 'root', password: 'dnt_test' });
      await $('#cd-tab-ssh').click();
      await $("//label[contains(.,'SSH tunnel')]/input").click();
      await $('#cd-ssh-host').setValue('127.0.0.1');
      await $('#cd-ssh-port').setValue('2222');
      await $('#cd-ssh-user').setValue('dnt');
      await $('#cd-ssh-auth').selectByAttribute('value', 'password');
      await $('#cd-ssh-password').setValue('dnt_test');
      await shot('conn-ssh-filled');
      await byId('conn-test').click();
      await byId('host-fingerprint').waitForDisplayed({ timeout: 30_000 });
      await shot('ssh-hostkey-dialog');
      await $('[data-trust]').click();
      await byId('test-result').waitForDisplayed({ timeout: 30_000 });
      await browser.pause(1500);
      await shot('ssh-test-ok');
      await byId('conn-save').click();
      await byId('conn-name').waitForDisplayed({ reverse: true, timeout: 15_000 });
    });

    // ---- password prompt ----
    await step('password-prompt', async () => {
      await (await treeRow('ask-pw')).doubleClick();
      await $("//h2[starts-with(.,'Password for')]").waitForDisplayed({ timeout: 15_000 });
      await shot('password-prompt');
      await $('#pp-db_password').setValue('wrong');
      await browser.keys(['Enter']);
      await $("//div[@role='dialog']//p[@role='alert']").waitForDisplayed({ timeout: 15_000 });
      await shot('password-prompt-error');
      await browser.keys(['Escape']);
    });

    // ---- error node ----
    await step('error-node', async () => {
      await (await treeRow('dead-server')).doubleClick();
      await browser.pause(4000);
      await shot('tree-error-node');
    });

    // ---- import / export ----
    await step('export', async () => {
      await $('[aria-label="Connection menu"]').click();
      await $("//button[@role='menuitem'][contains(.,'Export connections')]").click();
      await browser.pause(800);
      await shot('export-dialog');
      await browser.keys(['Escape']);
    });
    await step('import', async () => {
      await $('[aria-label="Connection menu"]').click();
      await $("//button[@role='menuitem'][contains(.,'Import connections')]").click();
      await browser.pause(800);
      await shot('import-dialog');
      await browser.keys(['Escape']);
    });

    // ---- tree: MySQL with dnt_big and PG ----
    await step('tree-mysql', async () => {
      await expandRow('shop-prod');
      await shot('tree-mysql-open');
      const t0 = Date.now();
      await expandRow('dnt_big');
      await expandRow('Tables');
      await (await treeRow('t_00001')).waitForDisplayed({ timeout: 30_000 });
      note(`dnt_big Tables (5000) expand: ${Date.now() - t0} ms`);
      await shot('tree-dnt-big');
      await rightClick('t_00001');
      await shot('ctx-menu-table');
      await browser.keys(['Escape']);
      const tree = await $('[role="tree"]');
      await tree.scrollIntoView();
      await browser.execute(() => {
        const t = document.querySelector('[role="tree"]') as HTMLElement;
        t.scrollTop = t.scrollHeight / 2;
      });
      await browser.pause(300);
      await shot('tree-dnt-big-scrolled');
      await $('input[aria-label="Filter objects"]').setValue('t_0420');
      await browser.pause(800);
      await shot('tree-filter');
      await $('input[aria-label="Filter objects"]').setValue('zzz-nothing');
      await browser.pause(800);
      await shot('tree-filter-no-match');
      await $('input[aria-label="Filter objects"]').click();
      await browser.keys(['Control', 'a']);
      await browser.keys(['Backspace']);
      await browser.pause(800);
      await shot('tree-filter-cleared');
      const rendered = (await $$('[data-testid="tree-row"]')).length;
      const top = await browser.execute(() => (document.querySelector('[role="tree"]') as HTMLElement).scrollTop);
      note(`tree rows rendered after clearing filter: ${rendered}, scrollTop ${top}`);
      await browser.execute(() => {
        const t = document.querySelector('[role="tree"]') as HTMLElement;
        t.scrollTop = 0;
        t.dispatchEvent(new Event('scroll'));
      });
      await browser.pause(500);
      await (await treeRow('shop-prod')).click();
      // Collapse dnt_big (double-click its row)
      await (await treeRow('dnt_big')).doubleClick();
      await browser.pause(300);
      await rightClick('shop-prod');
      await shot('ctx-menu-connection-open');
      await browser.keys(['Escape']);
      await (await $('[role="tree"]')).click({ x: 10, y: 380 }).catch(() => {});
    });
    await step('tree-empty-area-menu', async () => {
      await browser.execute(() => {
        const t = document.querySelector('[role="tree"]') as HTMLElement;
        const r = t.getBoundingClientRect();
        t.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true, clientX: r.left + 40, clientY: r.bottom - 20 }));
      });
      await browser.pause(300);
      await shot('ctx-menu-empty-area');
      await browser.keys(['Escape']);
    });
    await step('tree-pg', async () => {
      await expandRow('Analytics');
      await expandRow('analytics-pg');
      await expandRow('dnt_fixture');
      await browser.pause(800);
      await shot('tree-pg-open');
    });

    // ---- object tab ----
    await step('object-table', async () => {
      await expandRow('shop-prod');
      await expandRow('dnt_fixture');
      await browser.pause(500);
      const tables = await $$("//*[@data-testid='tree-row'][.//span[contains(@class,'label') and normalize-space(.)='Tables']]");
      // The dnt_fixture Tables folder of shop-prod is the last expanded one.
      for (const t of tables) {
        if ((await t.getAttribute('aria-expanded')) !== 'true') {
          await t.doubleClick();
          break;
        }
      }
      await browser.pause(800);
      await (await treeRow('orders')).doubleClick();
      await browser.pause(1500);
      await shot('object-info');
      await byId('object-tab-ddl').click();
      await browser.pause(1000);
      await shot('object-ddl');
      await $("//button[@role='tab'][.='Structure']").click();
      await browser.pause(1000);
      await shot('object-structure');
    });
    await step('object-view', async () => {
      const views = await $$("//*[@data-testid='tree-row'][.//span[contains(@class,'label') and normalize-space(.)='Views']]");
      for (const v of views) {
        if ((await v.getAttribute('aria-expanded')) !== 'true' && (await v.isDisplayed())) {
          await v.doubleClick();
          break;
        }
      }
      await browser.pause(800);
      await (await treeRow('v_customer_orders')).doubleClick();
      await browser.pause(1500);
      await shot('object-view-info');
      await byId('object-tab-ddl').click();
      await browser.pause(1000);
      await shot('object-view-ddl');
      await $("//button[@role='tab'][.='Structure']").click();
      await browser.pause(1000);
      await shot('object-view-structure');
    });

    // ---- query tab (MySQL Production) ----
    await step('query-empty', async () => {
      await newQueryOn('shop-prod');
      await inContextPicker(async () => {
        await byId('query-connection').selectByVisibleText('shop-prod');
        await browser.pause(300);
        await byId('query-database').selectByVisibleText('dnt_fixture');
      });
      await browser.pause(800);
      await shot('query-empty');
    });
    await step('autocomplete', async () => {
      await setSql('');
      await focusEditor();
      await browser.keys('SELECT * FROM '.split(''));
      await browser.keys(['Control', ' ']);
      await browser.pause(1200);
      await shot('autocomplete-tables');
      await browser.keys(['Escape']);
      await setSql('');
      await focusEditor();
      await browser.keys('SELECT o.'.split(''));
      await browser.pause(300);
      await browser.keys(['Escape']);
      await browser.keys(' FROM orders o'.split(''));
      // Move the caret back behind "o."
      for (let i = 0; i < ' FROM orders o'.length; i++) await browser.keys(['ArrowLeft']);
      await browser.keys(['Control', ' ']);
      await browser.pause(1200);
      await shot('autocomplete-alias-columns');
      await browser.keys(['Escape']);
    });
    await step('result-small', async () => {
      await setSql('SELECT * FROM orders');
      await runCurrent();
      await byId('result-summary').waitForDisplayed({ timeout: 30_000 });
      await idle();
      await shot('result-small');
    });
    await step('result-all-types', async () => {
      await setSql('SELECT * FROM all_types');
      await runCurrent();
      await idle();
      await browser.pause(500);
      await shot('result-all-types');
      await setSql('SELECT * FROM bin_blob');
      await runCurrent();
      await idle();
      await shot('result-blob');
    });
    await step('running-stop', async () => {
      await setSql('SELECT SLEEP(20)');
      await runCurrent();
      await browser.waitUntil(async () => await isRunning(), { timeout: 15_000 });
      await browser.pause(2500);
      await shot('query-running');
      await byId('query-stop').click();
      await browser.pause(300);
      await shot('query-stopping');
      await idle();
      await shot('query-stopped-messages');
    });
    await step('result-large', async () => {
      await setSql(
        'SELECT a.TABLE_SCHEMA, a.TABLE_NAME, a.COLUMN_NAME, a.DATA_TYPE, b.TABLE_NAME AS other\nFROM information_schema.COLUMNS a CROSS JOIN information_schema.COLUMNS b\nLIMIT 150000',
      );
      let t0 = Date.now();
      await runCurrent();
      await byId('result-summary').waitForDisplayed({ timeout: 60_000 });
      await idle();
      note(`150k-row SELECT with default limit: ${Date.now() - t0} ms`);
      await shot('result-truncated');
      await setRowLimit(1000000);
      t0 = Date.now();
      await runCurrent();
      await browser.pause(1500);
      await shot('result-large-streaming');
      await idle();
      note(`150k-row SELECT unlimited: ${Date.now() - t0} ms`);
      await shot('result-large');
      const grid = await byId('result-grid');
      await grid.scrollIntoView();
      const t1 = Date.now();
      for (let i = 0; i < 10; i++) {
        await browser.action('wheel').scroll({ origin: grid, deltaY: 3000 }).perform();
      }
      note(`10 wheel scrolls over 150k rows: ${Date.now() - t1} ms`);
      await browser.pause(400);
      await shot('result-large-scrolled');
      if (SIZE === 'narrow') {
        // Grow the window with a result on screen: does the grid repaint to the new size?
        await browser.setWindowSize(1440, 900);
        await browser.pause(1200);
        await evidence(`${P}-${String(++n).padStart(2, '0')}-grid-after-window-grow`);
        await browser.setWindowSize(1024, 700);
        await browser.pause(800);
      }
      await setRowLimit(10000);
    });
    await step('messages-errors', async () => {
      await toggleContinueOnError();
      await setSql(
        "SELECT 1;\nSELECT * FORM orders;\nSELECT CAST('abc' AS SIGNED);\nDO 1;\nSELECT * FROM no_such_table;\nSELECT 2",
      );
      await runAll();
      await confirmIfAsked('confirm-mixed-script');
      await idle();
      await byId('panel-messages').click();
      await browser.pause(500);
      await shot('messages-errors-warnings');
      await toggleContinueOnError();
      await setSql('SELECT 1;\nSELECT *\nFROM orders\nWHERE id = = 1;\nSELECT 3');
      await runAll();
      await idle();
      await browser.pause(800);
      await shot('editor-error-marker');
      // Hover the squiggle to see the marker message.
      const squiggle = await $('.monaco-editor .squiggly-error');
      if (await squiggle.isExisting()) {
        await squiggle.moveTo();
        await browser.pause(1500);
        await shot('editor-error-hover');
      }
    });
    await step('select-then-ok', async () => {
      await setSql('SELECT 1; SET @x = 1; SELECT @x');
      await runAll();
      await idle();
      await byId('panel-messages').click();
      await shot('messages-select-set-select');
    });
    await step('confirm-production', async () => {
      await setSql(
        "UPDATE orders SET status = status WHERE id = -1;\nSELECT 1;\nDELETE FROM orders WHERE id = -1;\nCREATE TABLE IF NOT EXISTS audit_tmp (id INT);\nDROP TABLE IF EXISTS audit_tmp;\nFLUSH TABLES",
      );
      await runAll();
      await byId('run-confirm').waitForDisplayed({ timeout: 15_000 });
      await shot('confirm-production');
      if (THEME === 'light' && SIZE === 'default') note(`confirm tab order: ${(await tabOrder(4)).join(' > ')}`);
      await byId('run-confirm-cancel').click();
      await setSql('DELETE FROM orders WHERE id = -1');
      await runCurrent();
      await byId('run-confirm').waitForDisplayed({ timeout: 15_000 });
      await shot('confirm-production-single');
      await browser.keys(['Escape']);
      await waitGone('run-confirm');
    });
    await step('explain-mysql', async () => {
      await setSql('SELECT o.id, c.name FROM orders o JOIN customers c ON c.id = o.customer_id WHERE o.status = \'paid\'');
      await byId('query-explain').click();
      await byId('explain-view').waitForDisplayed({ timeout: 30_000 });
      await browser.pause(800);
      await shot('explain-mysql');
      await byId('query-explain-menu').click();
      await browser.pause(300);
      await shot('explain-menu');
      await browser.keys(['Escape']);
      await $("//div[@data-testid='explain-view']//button[@role='tab'][.='Tree']").click();
      await shot('explain-mysql-tree');
      await $("//div[@data-testid='explain-view']//button[@role='tab'][.='JSON']").click();
      await shot('explain-mysql-raw');
    });
    await step('history', async () => {
      await byId('right-sidebar-history').click();
      await browser.pause(1000);
      await shot('history');
      await $('[data-testid="history-connection"]').selectByVisibleText('All connections');
      await browser.pause(800);
      await shot('history-all');
      await byId('right-sidebar-history').click();
    });
    await step('tx-active', async () => {
      await setSql('START TRANSACTION');
      await runCurrent();
      await idle();
      await setSql('UPDATE orders SET status = status WHERE id = 1');
      await runCurrent();
      await confirmIfAsked();
      await idle();
      await browser.pause(500);
      await shot('tx-active-statusbar');
      // Close the tab with the open transaction.
      const close = await $("//div[contains(@class,'tab') and contains(@class,'active')]//button[contains(@class,'tab-close')]");
      await close.click();
      await $("//h2[starts-with(.,'Close ')]").waitForDisplayed({ timeout: 10_000 });
      await shot('tx-close-dialog');
      await browser.keys(['Escape']);
      await browser.pause(300);
    });

    // ---- PG query tab ----
    await step('pg-query', async () => {
      // Select a node of analytics-pg so New query binds to it.
      const pgDb = await $$("//*[@data-testid='tree-row'][.//span[contains(@class,'label') and normalize-space(.)='dnt_fixture']]");
      for (const r of pgDb) {
        if ((await r.isDisplayed()) && (await r.getAttribute('aria-level')) === '3') {
          await r.click();
        }
      }
      await (await treeRow('analytics-pg')).click();
      await byId('new-query').click();
      await (await editor()).waitForDisplayed({ timeout: 30_000 });
      await setSql("DO $$ BEGIN RAISE NOTICE 'hello from plpgsql'; RAISE WARNING 'careful'; END $$;\nSELECT 1/0;\nSELECT 42 AS answer");
      await toggleContinueOnError();
      await runAll();
      await idle();
      await byId('panel-messages').click();
      await browser.pause(500);
      await shot('pg-messages-notice-error');
      await setSql('SELECT * FROM generate_series(1, 200000) g');
      await setRowLimit(1000000);
      const t0 = Date.now();
      await runCurrent();
      await idle();
      note(`PG 200k rows: ${Date.now() - t0} ms`);
      await shot('pg-result-200k');
      await inContextPicker(async () => {
        await byId('query-database').selectByVisibleText('dnt_fixture');
      });
      await setSql('SELECT c.name, count(*) FROM customers c JOIN customers d ON d.id <= c.id GROUP BY c.name ORDER BY 2 DESC');
      await byId('query-explain').click();
      await byId('explain-view').waitForDisplayed({ timeout: 30_000 });
      await browser.pause(800);
      await shot('explain-pg');
      await setSql('SELECT * FROM customers');
      await byId('query-explain').click();
      await browser.pause(1500);
      await shot('explain-pg-simple');
      await setSql('BEGIN;\nSELECT 1/0;');
      await runAll();
      await idle();
      await browser.pause(500);
      await shot('pg-tx-aborted');
      await setSql('ROLLBACK');
      await runCurrent();
      await idle();
    });

    // ---- read-only blocked ----
    await step('blocked', async () => {
      await (await treeRow('reporting-ro')).click();
      await byId('new-query').click();
      await (await editor()).waitForDisplayed({ timeout: 30_000 });
      await setSql('SELECT 1;\nDELETE FROM dnt_fixture.orders WHERE id = -1;\nDROP TABLE dnt_fixture.nothing');
      await runAll();
      await byId('run-blocked').waitForDisplayed({ timeout: 30_000 });
      await shot('blocked-readonly');
      await browser.keys(['Escape']);
    });

    // ---- long SQL and many tabs ----
    await step('long-sql', async () => {
      const cols = Array.from({ length: 60 }, (_, i) => `o.id + ${i} AS computed_column_number_${i}`).join(', ');
      await setSql(
        `-- a long generated statement\nSELECT ${cols}\nFROM dnt_fixture.orders o\nWHERE o.status IN ('new', 'paid', 'shipped')\n  AND o.total > 0\nORDER BY o.id;\n` +
          Array.from({ length: 40 }, (_, i) => `SELECT ${i} AS n${i};`).join('\n'),
      );
      await runAll();
      await idle();
      await shot('long-sql-run');
      await byId('panel-messages').click();
      await shot('long-sql-messages');
    });
    await step('many-tabs', async () => {
      for (let i = 0; i < 7; i++) {
        await byId('new-query').click();
        await browser.pause(300);
      }
      await shot('many-tabs');
    });

    // ---- keyboard: tree navigation focus ----
    await step('tree-keyboard', async () => {
      await (await treeRow('shop-prod')).click();
      await browser.keys(['ArrowDown']);
      await browser.keys(['ArrowDown']);
      await shot('tree-keyboard-focus');
    });

    // ---- app close with open transaction (last: may end the session) ----
    await step('app-close', async () => {
      await (await treeRow('shop-prod')).click();
      await byId('new-query').click();
      await (await editor()).waitForDisplayed({ timeout: 30_000 });
      await setSql('START TRANSACTION');
      await runCurrent();
      await idle();
      fs.writeFileSync(path.join(evidenceDir, `${P}-notes.txt`), notes.join('\n'));
      await shot('tx-open-before-quit');
      // The app-close dialog is not drivable: WebDriver's closeWindow drops the session.
    });

    fs.mkdirSync(evidenceDir, { recursive: true });
    fs.writeFileSync(path.join(evidenceDir, `${P}-notes.txt`), notes.join('\n'));
  });
});
