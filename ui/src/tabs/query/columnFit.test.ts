import { describe, expect, it } from 'vitest';
import { CELL_PAD, MAX_COL_WIDTH, bodyCharPx, charsForWidth, fitCellText, fitColumnWidth, type GridFont, type Measurer } from './format';

const font: GridFont = { body: '13px mono', header: '600 12px mono', marker: '11px mono' };

/** A monospace font with a fractional advance, like real fonts: 7.83 px per body character. */
const mono = (body = 7.83): Measurer => ({
  width(text, f) {
    const px = f === font.body ? body : f === font.header ? 7.2 : 6.6;
    return text.length * px;
  },
});

describe('column fit uses the measured font', () => {
  it('shows_a_value_in_full_that_the_estimate_used_to_clip', () => {
    // "shipped" is 7 characters: 54.81 px + padding rounds to 75 px, where the old estimate fit 6.
    const m = mono();
    const width = fitColumnWidth('status', 'varchar', ['shipped'], font, m);
    const max = charsForWidth(width, bodyCharPx(font.body, m));
    expect(max).toBeGreaterThanOrEqual(7);
    expect(fitCellText({ text: 'shipped', kind: 'text' }, max)).toBe('shipped');
  });

  it('never_clips_a_fitted_value_for_any_length_and_advance', () => {
    for (const advance of [7, 7.5, 7.83, 7.9, 8.04, 8.5]) {
      const m = mono(advance);
      for (let n = 1; n <= 40; n++) {
        const text = 'x'.repeat(n);
        const width = fitColumnWidth('c', 't', [text], font, m);
        if (width >= MAX_COL_WIDTH) continue;
        const max = charsForWidth(width, bodyCharPx(font.body, m));
        expect(max, `${n} chars at ${advance}px`).toBeGreaterThanOrEqual(n);
      }
    }
  });

  it('adds_the_cell_padding_and_caps_at_the_maximum', () => {
    const m = mono();
    expect(fitColumnWidth('c', 't', ['x'.repeat(30)], font, m)).toBe(Math.ceil(30 * 7.83) + CELL_PAD);
    expect(fitColumnWidth('c', 't', ['x'.repeat(500)], font, m)).toBe(MAX_COL_WIDTH);
  });

  it('measures_the_header_with_the_header_font', () => {
    const m = mono();
    expect(fitColumnWidth('a_long_header_name', 'int', [], font, m)).toBe(Math.ceil(18 * 7.2) + CELL_PAD);
  });

  it('falls_back_when_a_font_cannot_be_measured', () => {
    expect(bodyCharPx(font.body, { width: () => Number.NaN })).toBeGreaterThan(0);
  });
});
