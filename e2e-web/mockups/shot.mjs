import { chromium } from '@playwright/test';

const url = new URL('./new-connection.html', import.meta.url).href;
const browser = await chromium.launch();

async function shot(name, dark) {
  const page = await browser.newPage({ viewport: { width: 1280, height: 880 }, deviceScaleFactor: 2 });
  await page.goto(url);
  if (dark) {
    await page.evaluate(() => {
      document.body.classList.add('dark');
      const status = document.querySelector('.status');
      status.innerHTML =
        '<span class="checkmark" aria-hidden="true">✓</span> Connected to PostgreSQL 16.2 · Connect 38 ms · Detect server 4 ms';
    });
  }
  await page.screenshot({ path: `new-connection-${name}.png` });
  await page.close();
}

await shot('light', false);
await shot('dark', true);
await browser.close();
