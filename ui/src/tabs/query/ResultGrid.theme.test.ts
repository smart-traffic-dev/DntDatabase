import { afterEach, describe, expect, it } from 'vitest';
import { buildTheme } from './ResultGrid';

const root = document.documentElement;
const set = (vars: Record<string, string>) => {
  for (const [k, v] of Object.entries(vars)) root.style.setProperty(k, v);
};

afterEach(() => {
  root.removeAttribute('style');
  root.classList.remove('dark');
});

describe('result grid theme', () => {
  it('grid theme follows the dark class', () => {
    set({ '--dnt-accent': '#2563d9', '--dnt-bg-app': '#ffffff', '--dnt-fg': '#131315' });
    const light = buildTheme().theme;
    expect(light.accentColor).toBe('#2563d9');
    expect(light.bgCell).toBe('#ffffff');
    expect(light.textDark).toBe('#131315');

    root.classList.add('dark');
    set({ '--dnt-accent': '#6b9bf2', '--dnt-bg-app': '#1b1b1e', '--dnt-fg': '#ececee' });
    const dark = buildTheme().theme;
    expect(dark.accentColor).toBe('#6b9bf2');
    expect(dark.bgCell).toBe('#1b1b1e');
    expect(dark.textDark).toBe('#ececee');
  });

  it('falls back to the light palette when tokens are missing', () => {
    const t = buildTheme().theme;
    expect(t.accentColor).toBe('#2563d9');
    expect(t.bgCell).toBe('#ffffff');
  });
});
