import { chromium } from '@playwright/test';
import { fileURLToPath } from 'node:url';

const url = new URL('./explain.html', import.meta.url).href;
const out = (name) => fileURLToPath(new URL(name, import.meta.url));
const browser = await chromium.launch();
const page = await browser.newPage({ viewport: { width: 1440, height: 900 }, deviceScaleFactor: 2 });
page.on('pageerror', (error) => console.error('pageerror:', error));

await page.goto(url);
await page.waitForTimeout(300);

await page.screenshot({ path: out('explain-tree-light.png') });

await page.click('[data-view="table"]');
await page.waitForTimeout(120);
await page.screenshot({ path: out('explain-table-light.png') });

await page.click('[data-view="diagram"]');
await page.waitForTimeout(120);
await page.screenshot({ path: out('explain-diagram-light.png') });

await page.click('#themeToggle');
await page.click('[data-view="tree"]');
await page.waitForTimeout(120);
await page.screenshot({ path: out('explain-tree-dark.png') });

await browser.close();
