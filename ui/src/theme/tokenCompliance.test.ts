import { describe, expect, it } from 'vitest';

const sheets = import.meta.glob('../**/*.css', { query: '?raw', import: 'default', eager: true }) as Record<string, string>;

const strip = (raw: string) => raw.replace(/\/\*[\s\S]*?\*\//g, '');
const declarations = (css: string) => css.split(/[;{}]/).map((d) => d.trim()).filter(Boolean);
const appSheets = () => Object.entries(sheets).filter(([file]) => !file.endsWith('theme/tokens.css'));

describe('design token compliance', () => {
  it('no accent bars: border-left/top are hairlines only, no inset box-shadow stripes', () => {
    expect(appSheets().length).toBeGreaterThan(3);
    const offending: string[] = [];
    for (const [file, raw] of appSheets()) {
      for (const d of declarations(strip(raw))) {
        const border = /^border-(left|top)\s*:\s*(.*)$/.exec(d);
        if (border) {
          const value = border[2] ?? '';
          // `0`/`none` and 1px hairlines are structural separators.
          if (!/^(0|none)$/.test(value) && !/^1px\s/.test(value)) offending.push(`${file}: ${d}`);
        }
        const shadow = /^box-shadow\s*:\s*inset\s+(.*)$/.exec(d);
        // `inset 0 0 0 Npx` is a full ring (selected option), not a one-sided stripe.
        if (shadow && !/^0\s+0\s+0\s/.test(shadow[1] ?? '')) offending.push(`${file}: ${d}`);
      }
    }
    expect(offending).toEqual([]);
  });

  it('no raw colours outside token definitions (backdrop rgba and danger button #fff allowed)', () => {
    const allowed = [/^background\s*:\s*rgba\(0,\s*0,\s*0,\s*0\.\d+\)$/, /^color\s*:\s*#fff$/];
    const offending: string[] = [];
    for (const [file, raw] of appSheets()) {
      for (const d of declarations(strip(raw))) {
        if (!/#[0-9a-fA-F]{3,8}\b|rgba?\(/.test(d)) continue;
        // Custom property definitions (the app aliases in theme.css) are where raw colours live.
        if (d.startsWith('--')) continue;
        if (allowed.some((re) => re.test(d))) continue;
        offending.push(`${file}: ${d}`);
      }
    }
    expect(offending).toEqual([]);
  });

  /** Every `selector { body }` rule (top level or inside an at-rule) of a stylesheet. */
  const rules = (css: string) =>
    [...strip(css).matchAll(/([^{}]+)\{([^{}]*)\}/g)].map((m) => ({ selector: (m[1] ?? '').trim(), body: m[2] ?? '' }));

  it('disabled is never opacity', () => {
    const offending: string[] = [];
    for (const [file, raw] of appSheets()) {
      for (const r of rules(raw)) {
        if (r.selector.includes(':disabled') && !r.selector.includes(':not(:disabled)') && /(^|[;\s])opacity\s*:/.test(r.body)) {
          offending.push(`${file}: ${r.selector}`);
        }
      }
    }
    expect(offending).toEqual([]);
  });

  it('focus is at most 1px and never a ring', () => {
    const offending: string[] = [];
    for (const [file, raw] of appSheets()) {
      for (const r of rules(raw)) {
        for (const d of declarations(r.body)) {
          const outline = /^outline\s*:\s*(\d+(?:\.\d+)?)px/.exec(d);
          if (outline && Number(outline[1]) > 1) offending.push(`${file}: ${r.selector}: ${d}`);
          if (r.selector.includes(':focus') && /^box-shadow\s*:/.test(d) && !/^box-shadow\s*:\s*none$/.test(d)) {
            offending.push(`${file}: ${r.selector}: ${d}`);
          }
        }
      }
    }
    expect(offending).toEqual([]);
  });

  it('focus is visible on rows and menu items', () => {
    const theme = sheets['../theme.css'] ?? '';
    expect(theme.length).toBeGreaterThan(1000);
    const forSelector = (sel: string) => rules(theme).filter((r) => r.selector.split(',').map((s) => s.trim()).includes(sel));
    for (const sel of ["[role='menuitem']:focus-visible", "[role='option']:focus-visible"]) {
      const matched = forSelector(sel);
      expect(matched.some((r) => /outline\s*:\s*1px solid var\(--dnt-focus\)/.test(r.body)), sel).toBe(true);
      expect(matched.some((r) => /outline\s*:\s*none/.test(r.body)), sel).toBe(false);
    }
  });

  // #2563d9 on the selection fill is 4.36:1, under AA for the small labels that sit on it.
  it('accent text on the selection fill uses the darker accent-hover', () => {
    const offending: string[] = [];
    const selFill = /background(-color)?\s*:\s*var\(--(sel|dnt-bg-selected)\)/;
    const accentText = /(^|[;\s])color\s*:\s*var\(--(accent|dnt-accent)\)/;
    for (const [file, raw] of appSheets()) {
      for (const r of rules(raw)) if (selFill.test(r.body) && accentText.test(r.body)) offending.push(`${file}: ${r.selector}`);
    }
    const palette = sheets['../components/palette.css'] ?? '';
    for (const r of rules(palette)) {
      if (r.selector.includes('.active') && r.selector.includes('.palette-match') && !/var\(--dnt-accent-hover\)/.test(r.body)) {
        offending.push(`palette.css: ${r.selector}`);
      }
    }
    if (!rules(palette).some((r) => r.selector.includes('.active') && r.selector.includes('.palette-match'))) {
      offending.push('palette.css: no rule for .palette-row.active .palette-match');
    }
    expect(offending).toEqual([]);
  });

  it('font-size, font-family, radius and shadow use tokens', () => {
    const offending: string[] = [];
    for (const [file, raw] of appSheets()) {
      for (const d of declarations(strip(raw))) {
        if (d.startsWith('--')) continue;
        if (/^font-size\s*:\s*[\d.]+px/.test(d)) offending.push(`${file}: ${d}`);
        if (/^font-family\s*:/.test(d) && !/var\(--font-(sans|mono)\)/.test(d)) offending.push(`${file}: ${d}`);
        if (/^border-radius\s*:/.test(d) && /\d+px/.test(d)) offending.push(`${file}: ${d}`);
        if (/^box-shadow\s*:/.test(d) && !/var\(--|inset 0 0 0 1px|^box-shadow\s*:\s*none/.test(d)) offending.push(`${file}: ${d}`);
      }
    }
    expect(offending).toEqual([]);
  });
});
