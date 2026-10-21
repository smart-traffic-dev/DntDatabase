import { describe, expect, it } from 'vitest';
import raw from './tokens.css?raw';

type Block = Map<string, string>;

/** The custom properties declared directly in each top-level rule whose selector is `selector`. */
function block(css: string, selector: string): Block {
  const out: Block = new Map();
  const clean = css.replace(/\/\*[\s\S]*?\*\//g, '');
  for (const m of clean.matchAll(/([^{}]+)\{([^{}]*)\}/g)) {
    const selectors = (m[1] ?? '').split(',').map((s) => s.trim());
    if (!selectors.includes(selector)) continue;
    for (const d of (m[2] ?? '').matchAll(/(--[\w-]+)\s*:\s*([^;]+);/g)) out.set(d[1] ?? '', (d[2] ?? '').trim());
  }
  return out;
}

const light = block(raw, ':root');
const dark = new Map([...light, ...block(raw, '.dark')]);
const comfortable = new Map([...light, ...block(raw, "[data-density='comfortable']")]);

/** Follows `var(--x)` chains to the final value. */
function resolve(vars: Block, name: string, depth = 0): string {
  const v = vars.get(name);
  if (v === undefined) throw new Error(`${name} is not defined`);
  if (depth > 20) throw new Error(`${name}: var() cycle`);
  const m = /^var\(\s*(--[\w-]+)\s*\)$/.exec(v);
  return m ? resolve(vars, m[1] ?? '', depth + 1) : v.toLowerCase();
}

function luminance(hex: string): number {
  const h = hex.replace('#', '');
  const full = h.length === 3 ? [...h].map((c) => c + c).join('') : h;
  const [r, g, b] = [0, 2, 4].map((i) => parseInt(full.slice(i, i + 2), 16) / 255).map((c) =>
    c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4,
  );
  return 0.2126 * (r ?? 0) + 0.7152 * (g ?? 0) + 0.0722 * (b ?? 0);
}

function contrast(a: string, b: string): number {
  const [hi, lo] = [luminance(a), luminance(b)].sort((x, y) => y - x);
  return ((hi ?? 0) + 0.05) / ((lo ?? 0) + 0.05);
}

const PRIMITIVE = /^--dnt-(gray|blue|red|amber|green)-\d+$/;
const COMPONENT = /^--dnt-(ctl-h|row-h|titlebar-h|tab-h|statusbar-h|icon-size|space|radius|text|dur|ease|z)-?/;

describe('design tokens', () => {
  it('accent is blue in both themes', () => {
    expect(resolve(light, '--dnt-accent')).toBe('#2563d9');
    expect(resolve(dark, '--dnt-accent')).toBe('#6b9bf2');
  });

  it('every semantic token is defined for light and dark', () => {
    const semantic = [...light.keys()].filter((n) => n.startsWith('--dnt-') && !PRIMITIVE.test(n) && !COMPONENT.test(n));
    expect(semantic.length).toBeGreaterThan(30);
    const darkOwn = block(raw, '.dark');
    expect(semantic.filter((n) => !darkOwn.has(n))).toEqual([]);
  });

  it('secondary text meets 4.5:1 on chrome', () => {
    for (const vars of [light, dark]) {
      const chrome = resolve(vars, '--dnt-bg-chrome');
      for (const fg of ['--dnt-fg-muted', '--dnt-fg-subtle']) {
        expect(contrast(resolve(vars, fg), chrome), fg).toBeGreaterThanOrEqual(4.5);
      }
    }
  });

  it('accent-hover text meets 4.5:1 on the selection fill', () => {
    // Dark selection is translucent: blend it over bg-app first.
    const blend = (rgba: string, under: string) => {
      const m = /rgba\((\d+),\s*(\d+),\s*(\d+),\s*([\d.]+)\)/.exec(rgba);
      if (!m) return rgba;
      const u = under.replace('#', '');
      const a = Number(m[4]);
      return `#${[1, 2, 3]
        .map((i, k) => Math.round(Number(m[i]) * a + parseInt(u.slice(k * 2, k * 2 + 2), 16) * (1 - a)))
        .map((n) => n.toString(16).padStart(2, '0'))
        .join('')}`;
    };
    for (const vars of [light, dark]) {
      const fill = blend(resolve(vars, '--dnt-bg-selected'), resolve(vars, '--dnt-bg-app'));
      expect(contrast(resolve(vars, '--dnt-accent-hover'), fill)).toBeGreaterThanOrEqual(4.5);
    }
  });

  it('comfortable density raises control heights', () => {
    expect(resolve(light, '--dnt-ctl-h-md')).toBe('28px');
    expect(resolve(comfortable, '--dnt-ctl-h-md')).toBe('30px');
    expect(resolve(comfortable, '--dnt-row-h-grid')).toBe('30px');
  });

  it('legacy names still resolve to the new values', () => {
    expect(resolve(light, '--studio-accent')).toBe('#2563d9');
    expect(resolve(dark, '--studio-accent')).toBe('#6b9bf2');
    expect(resolve(light, '--accent')).toBe('#2563d9');
    expect(resolve(light, '--studio-selected')).toBe(resolve(light, '--dnt-bg-selected'));
    expect(resolve(light, '--text-callout')).toBe('13px');
  });
});
