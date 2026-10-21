import { describe, expect, it } from 'vitest';
import { toMonacoColor } from './dntTheme';

// Monaco parses theme colours with Color.fromHex: anything that is not #rgb / #rrggbb / #rrggbbaa turns red.
describe('toMonacoColor', () => {
  it('turns a translucent rgba() into #rrggbbaa', () => {
    expect(toMonacoColor('rgba(59, 120, 231, 0.28)')).toBe('#3b78e747');
  });

  it('turns an opaque rgb() into #rrggbb', () => {
    expect(toMonacoColor('rgb(27, 27, 30)')).toBe('#1b1b1e');
  });

  it('keeps hex values', () => {
    expect(toMonacoColor('#2563D9')).toBe('#2563d9');
  });

  it('returns null for what it cannot parse', () => {
    expect(toMonacoColor('color-mix(in srgb, red 10%, blue)')).toBeNull();
  });
});
