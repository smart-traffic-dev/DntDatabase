import { chromium } from '@playwright/test';
import { mkdirSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

// Screenshots every board of modals-v2.html (light, plus dark with `dark`), one PNG per board.
// Usage: node shot-modals-v2.mjs [boardIds…] [dark]
const here = (p) => fileURLToPath(new URL(p, import.meta.url));
const args = process.argv.slice(2);
const dark = args.includes('dark');
const only = args.filter((a) => a !== 'dark');
mkdirSync(here('./modals-v2-shots'), { recursive: true });

const browser = await chromium.launch();
const page = await browser.newPage({ viewport: { width: 1600, height: 1000 }, deviceScaleFactor: 1 });
await page.goto(new URL('./modals-v2.html', import.meta.url).href + (dark ? '#dark' : ''));
await page.waitForLoadState('load');
// The sticky top bar would overlap element screenshots.
await page.addStyleTag({ content: '.top { position: static !important; }' });
const ids = only.length ? only : await page.$$eval('section.board', (s) => s.map((e) => e.id));
for (const id of ids) {
  await page.locator(`#${id}`).screenshot({ path: here(`./modals-v2-shots/${id}${dark ? '-dark' : ''}.png`) });
}
await browser.close();
console.log(`shot ${ids.join(' ')}${dark ? ' (dark)' : ''}`);
