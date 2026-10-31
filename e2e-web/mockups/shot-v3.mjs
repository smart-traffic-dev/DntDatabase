import { chromium } from '@playwright/test';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

// Brand logos (simple-icons, CC0) are inlined so they can be tinted with CSS fill.
const here = (p) => fileURLToPath(new URL(p, import.meta.url));
const logos = Object.fromEntries(
  ['mysql', 'mariadb', 'postgresql'].map((n) => [n, readFileSync(here(`./logos/${n}.svg`), 'utf8')]),
);
const html = readFileSync(here('./new-connection-v3.html'), 'utf8');

const browser = await chromium.launch();
const shots = [['1', false], ['2', false], ['2b', false], ['3', false], ['4', false], ['2b', true]];
for (const [step, dark] of shots) {
  const page = await browser.newPage({ viewport: { width: 1000, height: 700 }, deviceScaleFactor: 1.5 });
  await page.goto(new URL('./new-connection-v3.html', import.meta.url).href);
  // Re-run the inline script with the requested step.
  await page.setContent(html.replace('data-step="1"', `data-step="${step}"${dark ? ' class="dark"' : ''}`), {
    waitUntil: 'load',
  });
  await page.addStyleTag({ path: here('../../ui/src/theme/tokens.css') });
  await page.evaluate((logos) => {
    document.querySelectorAll('[data-logo]').forEach((el) => (el.innerHTML = logos[el.dataset.logo]));
  }, logos);
  await page.screenshot({ path: here(`./new-connection-v3-${step}${dark ? '-dark' : ''}.png`) });
  await page.close();
}
await browser.close();
