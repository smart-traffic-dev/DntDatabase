import type { Page } from '@playwright/test';
import type { AiCompleteRequest, AiSettings, ConnectionConfig, Tree } from '../../ui/src/bindings';
import { config, ids } from '../../ui/src/test/fixtures/data';
import { byId, pickOption } from '../src/app';
import { createBackend } from '../src/backend';
import { installBackend } from '../src/shim';
import { expect, test } from '../src/test';

// AI ghost text in a real Monaco (AI editor completion spec §5, §9.6): the fake backend answers
// `ai_complete`; the editor shows the suggestion as ghost text, Tab accepts it. SQL and MongoDB
// tabs get suggestions, Redis tabs none.

const tree = (): Tree => ({
  groups: [],
  connections: [
    {
      config: config(ids.prod, 'shop', { env_tag: 'local', default_db: 'dnt_fixture' }).config,
      has_db_password: true,
      has_ssh_secret: false,
      last_connected_at: null,
    },
  ],
});

const settings = (trigger: 'automatic' | 'manual'): AiSettings => ({
  config: {
    base_url: 'https://api.example.com/v1',
    model: 'coder',
    trigger,
    max_tokens: 128,
    timeout_ms: 5000,
    max_lines: 8,
  },
  has_key: true,
  origin: 'https://api.example.com',
  host: 'api.example.com',
  consented: true,
  consent_covers_mongo: true,
});

async function openQuery(page: Page, trigger: 'automatic' | 'manual', requests: AiCompleteRequest[]) {
  const backend = createBackend({
    list_tree: tree,
    ai_settings_get: () => settings(trigger),
    ai_connections: () => [],
    ai_complete: (args) => {
      const request = args.request as AiCompleteRequest;
      requests.push(request);
      return request.text.trimEnd().endsWith('WHERE') ? { kind: 'suggestion', text: 'status = 1' } : { kind: 'empty' };
    },
  });
  await installBackend(page, backend.handlers);
  await page.goto('/');
  await byId(page, 'new-query').click();
  await expect(byId(page, 'query-editor')).toBeVisible();
  await byId(page, 'query-context').click();
  await pickOption(page, 'query-connection', 'shop');
  await page.keyboard.press('Escape');
}

// Monaco renders spaces as U+00A0.
const editorText = async (page: Page) =>
  (await page.locator('[data-testid="query-editor"] .view-lines').innerText()).replace(/ /g, ' ');
const ghost = (page: Page) => page.locator('[data-testid="query-editor"]').locator('.ghost-text-decoration, .ghost-text');

async function typeInEditor(page: Page, text: string) {
  await page.locator('[data-testid="query-editor"] .view-lines').click();
  await page.keyboard.type(text);
}

test.describe('AI suggestions in the query editor', () => {
  test('a pause in typing shows ghost text and Tab accepts it', async ({ page }) => {
    const requests: AiCompleteRequest[] = [];
    await openQuery(page, 'automatic', requests);
    await expect(byId(page, 'status-ai')).toHaveText('AI');

    await typeInEditor(page, 'SELECT * FROM orders WHERE ');
    await expect(ghost(page).first()).toBeVisible();
    await expect(ghost(page).first()).toContainText('status');

    const last = requests.at(-1)!;
    expect(last.conn_id).toBe(ids.prod);
    expect(last.text).toBe('SELECT * FROM orders WHERE ');
    expect(last.cursor).toBe('SELECT * FROM orders WHERE '.length);

    await page.keyboard.press('Tab');
    await expect.poll(() => editorText(page)).toContain('WHERE status = 1');
    await expect(ghost(page)).toHaveCount(0);
  });

  test('typing what is suggested keeps the rest, and Tab still takes it', async ({ page }) => {
    const requests: AiCompleteRequest[] = [];
    await openQuery(page, 'automatic', requests);
    await typeInEditor(page, 'SELECT * FROM orders WHERE ');
    await expect(ghost(page).first()).toBeVisible();
    const asked = requests.length;

    // "st" would open the word suggest list (START TRANSACTION…) and take Tab.
    await page.keyboard.type('st');
    await page.waitForTimeout(500);
    await expect(ghost(page).first()).toContainText('atus');
    await expect(page.locator('[data-testid="query-editor"] .suggest-widget.visible')).toHaveCount(0);
    expect(requests).toHaveLength(asked);

    await page.keyboard.press('Tab');
    await expect.poll(() => editorText(page)).toContain('WHERE status = 1');
  });

  test('Esc dismisses the suggestion', async ({ page }) => {
    await openQuery(page, 'automatic', []);
    await typeInEditor(page, 'SELECT * FROM orders WHERE ');
    await expect(ghost(page).first()).toBeVisible();
    await page.keyboard.press('Escape');
    await expect(ghost(page)).toHaveCount(0);
    expect(await editorText(page)).not.toContain('status = 1');
  });

  test('manual mode asks only on Alt+\\', async ({ page }) => {
    const requests: AiCompleteRequest[] = [];
    await openQuery(page, 'manual', requests);
    await typeInEditor(page, 'SELECT * FROM orders WHERE ');
    // Longer than the typing pause: still nothing asked.
    await page.waitForTimeout(800);
    expect(requests).toHaveLength(0);
    await expect(ghost(page)).toHaveCount(0);

    await page.keyboard.press('Alt+Backslash');
    await expect(ghost(page).first()).toBeVisible();
    expect(requests).toHaveLength(1);
  });
});

const commandOptions = (driver: ConnectionConfig['options']['driver']): ConnectionConfig['options'] => ({
  version: 1,
  connect_timeout_s: 10,
  ssh_timeout_s: 10,
  charset: null,
  application_name: null,
  query_timeout_s: 0,
  driver,
});

test.describe('AI suggestions in a MongoDB query tab', () => {
  test('a pause in typing shows ghost text, Tab accepts it; a Redis tab asks nothing', async ({ page }) => {
    const requests: AiCompleteRequest[] = [];
    const backend = createBackend({
      ai_settings_get: () => settings('automatic'),
      ai_connections: () => [],
      ai_complete: (args) => {
        const request = args.request as AiCompleteRequest;
        requests.push(request);
        return request.text.endsWith('})') ? { kind: 'suggestion', text: '.sort({ total: -1 })' } : { kind: 'empty' };
      },
    });
    await backend.handlers.create_connection!({
      cfg: config(ids.legacy, 'docs', {
        kind: 'mongo',
        port: 27017,
        username: '',
        default_db: 'shop',
        options: commandOptions({ kind: 'mongo', srv: false, auth_source: null, replica_set: null, direct_connection: false, extra_hosts: [] }),
      }).config,
    });
    await backend.handlers.create_connection!({
      cfg: config(ids.analytics, 'cache', { kind: 'redis', port: 6379, username: '', options: commandOptions({ kind: 'redis', db: 0 }) }).config,
    });
    await installBackend(page, backend.handlers);
    await page.goto('/');

    await byId(page, 'new-query').click();
    await byId(page, 'query-context').click();
    await pickOption(page, 'query-connection', 'docs');
    await page.keyboard.press('Escape');
    await expect(page.getByTestId('query-editor').locator('[data-mode-id="mongo"]')).toHaveCount(1);
    await expect(byId(page, 'status-ai')).toHaveText('AI');

    const text = 'db.orders.find({ status: "paid" })';
    await typeInEditor(page, text);
    await expect(ghost(page).first()).toBeVisible();
    await expect(ghost(page).first()).toContainText('sort');
    // The same request as for SQL: the tab's database (the connection's default), no schema.
    expect(requests.at(-1)).toMatchObject({ conn_id: ids.legacy, database: 'shop', schema: null, text, cursor: text.length });

    await page.keyboard.press('Tab');
    await expect.poll(() => editorText(page)).toContain('db.orders.find({ status: "paid" }).sort({ total: -1 })');
    await expect(ghost(page)).toHaveCount(0);

    // A Redis tab gets no AI suggestions.
    const asked = requests.length;
    await byId(page, 'new-query').click();
    await byId(page, 'query-context').click();
    await pickOption(page, 'query-connection', 'cache');
    await page.keyboard.press('Escape');
    await typeInEditor(page, 'GET a');
    await page.keyboard.press('Alt+Backslash');
    await page.waitForTimeout(800);
    expect(requests.slice(asked).filter((r) => r.conn_id === ids.analytics)).toEqual([]);
    await expect(ghost(page)).toHaveCount(0);
  });
});
