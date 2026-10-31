import { chromium } from '@playwright/test';

const url = new URL('./explain.html', import.meta.url).href;
const browser = await chromium.launch();
const page = await browser.newPage({ viewport: { width: 1440, height: 900 }, deviceScaleFactor: 1 });
page.on('pageerror', (e) => console.error('PAGEERROR', e));
await page.goto(url);
await page.waitForTimeout(300);

const metrics = await page.evaluate(() => {
  const rect = (sel) => {
    const el = document.querySelector(sel);
    if (!el) return null;
    const r = el.getBoundingClientRect();
    return {
      x: Math.round(r.x),
      y: Math.round(r.y),
      w: Math.round(r.width),
      h: Math.round(r.height),
      sw: el.scrollWidth,
      sh: el.scrollHeight,
      cw: el.clientWidth,
      ch: el.clientHeight,
      overflowX: el.scrollWidth > el.clientWidth + 1,
      overflowY: el.scrollHeight > el.clientHeight + 1,
    };
  };
  const overflows = [...document.querySelectorAll('*')]
    .filter((el) => el.scrollWidth > el.clientWidth + 1 && el.clientWidth > 0)
    .slice(0, 20)
    .map((el) => ({
      tag: el.tagName,
      cls: el.className?.toString().slice(0, 60),
      sw: el.scrollWidth,
      cw: el.clientWidth,
    }));
  return {
    body: { sw: document.body.scrollWidth, cw: document.body.clientWidth, sh: document.body.scrollHeight, ch: document.body.clientHeight },
    app: rect('.app'),
    editor: rect('.editor-zone'),
    result: rect('.result-panel'),
    toolbar: rect('.ex-toolbar'),
    summary: rect('.summary'),
    main: rect('.ex-main'),
    tree: rect('#tree'),
    head: rect('.plan-head'),
    inspector: rect('.inspector'),
    inspScroll: rect('.insp-scroll'),
    rows: [...document.querySelectorAll('#tree .row')].map((r) => ({
      id: r.dataset.id,
      hidden: r.hidden,
      h: Math.round(r.getBoundingClientRect().height),
      w: Math.round(r.getBoundingClientRect().width),
    })),
    overflows,
  };
});

console.log(JSON.stringify(metrics, null, 2));
await browser.close();
