import { describe, expect, it, vi } from 'vitest';
import { applyErrorMarkers, finishedGlyphs, highlightRange, type EditorLike, type MonacoLike } from './editorMarks';

// A model of "ab😀\ncd" whose offsets are UTF-16 code units, like Monaco's.
const TEXT = 'ab😀\ncd';
const model = {
  getPositionAt(offset: number) {
    const before = TEXT.slice(0, offset);
    const line = before.split('\n').length;
    return { lineNumber: line, column: before.length - before.lastIndexOf('\n') };
  },
  getWordAtPosition: () => null,
  getValueLength: () => TEXT.length,
};

describe('editor marks', () => {
  it('error_markers_set_on_model', () => {
    const setModelMarkers = vi.fn();
    const monaco = { editor: { setModelMarkers }, MarkerSeverity: { Error: 8 } } as unknown as MonacoLike;
    applyErrorMarkers(monaco, model as never, [{ offset: 5, message: 'syntax error' }]);
    expect(setModelMarkers).toHaveBeenCalledTimes(1);
    const [m, owner, markers] = setModelMarkers.mock.calls[0]!;
    expect(m).toBe(model);
    expect(owner).toBe('dnt-query');
    expect(markers).toEqual([
      { severity: 8, message: 'syntax error', startLineNumber: 2, startColumn: 1, endLineNumber: 2, endColumn: 2 },
    ]);
  });

  it('no_markers_clears_them', () => {
    const setModelMarkers = vi.fn();
    applyErrorMarkers({ editor: { setModelMarkers }, MarkerSeverity: { Error: 8 } } as unknown as MonacoLike, model as never, []);
    expect(setModelMarkers.mock.calls[0]![2]).toEqual([]);
  });

  it('highlight_range_uses_utf16_offsets', () => {
    expect(highlightRange(model as never, [2, 6])).toEqual({
      startLineNumber: 1,
      startColumn: 3,
      endLineNumber: 2,
      endColumn: 2,
    });
  });

  it('finished_glyphs_on_first_line_of_each_finished_statement', () => {
    const plan = {
      stmts: [
        { index: 0, range: [0, 4] as [number, number] },
        { index: 1, range: [5, 7] as [number, number] },
        { index: 2, range: [5, 7] as [number, number] },
      ],
    };
    const glyphs = finishedGlyphs(model as never, plan as never, [0, 1]);
    expect(glyphs.map((g) => g.range.startLineNumber)).toEqual([1, 2]);
    expect(glyphs[0]!.options.glyphMarginClassName).toBe('stmt-done');
  });

  it('editor_like_type_is_exported', () => {
    const e: EditorLike | null = null;
    expect(e).toBeNull();
  });
});
