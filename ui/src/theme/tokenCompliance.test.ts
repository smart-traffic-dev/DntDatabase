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
