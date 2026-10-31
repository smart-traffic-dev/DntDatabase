import { chromium } from '@playwright/test';

const url = new URL('./new-connection-v2.html', import.meta.url).href;
const browser = await chromium.launch();
for (const [name, dark] of [['light', false], ['dark', true]]) {
  const page = await browser.newPage({ viewport: { width: 1000, height: 820 }, deviceScaleFactor: 1.5 });
  await page.goto(url);
  if (dark) await page.evaluate(() => document.body.classList.add('dark'));
  await page.screenshot({ path: new URL(`./new-connection-v2-${name}.png`, import.meta.url).pathname.slice(1) });
  await page.close();
}
await browser.close();
