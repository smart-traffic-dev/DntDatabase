import { describe, expect, it } from 'vitest';

const sheets = import.meta.glob('../**/*.css', { query: '?raw', import: 'default', eager: true }) as Record<string, string>;

describe('CSS custom properties', () => {
  it('every var(--x) used in app CSS is defined in tokens.css or theme.css', () => {
    // Guard against a vacuous pass: ?raw CSS must not be stripped by the test config.
    expect(Object.values(sheets).join('').length).toBeGreaterThan(1000);
    const defined = new Set<string>();
    for (const css of Object.values(sheets)) {
      for (const m of css.matchAll(/(--[\w-]+)\s*:/g)) defined.add(m[1] ?? '');
    }
    const undefinedUses: string[] = [];
    for (const [file, raw] of Object.entries(sheets)) {
      const css = raw.replace(/\/\*[\s\S]*?\*\//g, '');
      for (const m of css.matchAll(/var\(\s*(--[\w-]+)\s*(,)?/g)) {
        // A var() with a fallback may reference an optional name.
        if (!defined.has(m[1] ?? '') && !m[2]) undefinedUses.push(`${file}: ${m[1]}`);
      }
    }
    expect(undefinedUses).toEqual([]);
  });

  // --danger / --production are fill and stripe colours; as text they miss AA contrast on the
  // tinted backgrounds. Text uses --danger-fg / --production-fg.
  it('text colour never uses the low-contrast --danger / --production', () => {
    const offending: string[] = [];
    for (const [file, raw] of Object.entries(sheets)) {
      const css = raw.replace(/\/\*[\s\S]*?\*\//g, '');
      for (const m of css.matchAll(/(?:^|[;{\s])color\s*:\s*([^;}]*)/g)) {
        if (/var\(\s*--(?:danger|production)\s*[),]/.test(m[1] ?? '')) offending.push(`${file}: color: ${m[1]}`);
      }
    }
    expect(offending).toEqual([]);
  });
});

