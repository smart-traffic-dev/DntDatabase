import { chromium } from '@playwright/test';

const url = new URL('./explain.html', import.meta.url).href;
const browser = await chromium.launch();
const page = await browser.newPage({ viewport: { width: 1440, height: 900 }, deviceScaleFactor: 1 });
page.on('pageerror', (e) => console.error('PAGEERROR', e));
await page.goto(url);
await page.waitForTimeout(300);

async function report(view) {
  await page.click(`[data-view="${view}"]`);
  await page.waitForTimeout(80);
  const m = await page.evaluate(() => {
    const r = (sel) => {
      const el = document.querySelector(sel);
      if (!el) return null;
      const b = el.getBoundingClientRect();
      return {
        x: Math.round(b.x),
        y: Math.round(b.y),
        w: Math.round(b.width),
        h: Math.round(b.height),
        sw: el.scrollWidth,
        sh: el.scrollHeight,
        cw: el.clientWidth,
        ch: el.clientHeight,
        overflowX: el.scrollWidth > el.clientWidth + 1,
        overflowY: el.scrollHeight > el.clientHeight + 1,
      };
    };
    return {
      tableWrap: r('.table-wrap'),
      table: r('.plan-table'),
      diagramWrap: r('.diagram-wrap'),
      diagramScroll: r('.diagram-scroll'),
      diagram: r('#diagram'),
      raw: r('.raw-wrap'),
      inspector: r('.inspector'),
      mainClass: document.querySelector('.ex-main').className,
      visibleView: [...document.querySelectorAll('.view')].filter((v) => v.classList.contains('on')).map((v) => v.id),
    };
  });
  console.log(view, JSON.stringify(m, null, 2));
}

await report('table');
await report('diagram');
await report('raw');
await report('tree');

await page.click('#themeToggle');
await page.waitForTimeout(80);
const dark = await page.evaluate(() => ({
  dark: document.body.classList.contains('dark'),
  bg: getComputedStyle(document.body).backgroundColor,
  fg: getComputedStyle(document.querySelector('.node-label')).color,
  panel: getComputedStyle(document.querySelector('.result-panel')).backgroundColor,
}));
console.log('dark', JSON.stringify(dark));
await browser.close();
