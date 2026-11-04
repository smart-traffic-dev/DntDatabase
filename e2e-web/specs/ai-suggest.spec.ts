import type { Page } from '@playwright/test';
import type { AiCompleteRequest, AiSettings, Tree } from '../../ui/src/bindings';
import { config, ids } from '../../ui/src/test/fixtures/data';
import { byId } from '../src/app';
import { createBackend } from '../src/backend';
import { installBackend } from '../src/shim';
import { expect, test } from '../src/test';

// AI ghost text in a real Monaco (AI editor completion spec §5, §9.6): the fake backend answers
// `ai_complete`; the editor shows the suggestion as ghost text, Tab accepts it.

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
});

async function openQuery(page: Page, trigger: 'automatic' | 'manual', requests: AiCompleteRequest[]) {
  const backend = createBackend({
    list_tree: tree,
    ai_settings_get: () => settings(trigger),
    ai_opt_in_list: () => [{ connection_id: ids.prod, name: 'shop', granted_at: 'now', active: true }],
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
  await byId(page, 'query-connection').selectOption({ label: 'shop' });
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
