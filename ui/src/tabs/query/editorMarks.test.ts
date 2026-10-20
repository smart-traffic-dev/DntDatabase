import { renderHook } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import type { PlanView } from '../../bindings';
import { emptyRun, reduceRun, startRun, type TabRun } from '../../store/queryRun';
import { appError, mockBackend, resetStores } from '../../test/backend';
import {
  applyErrorMarkers,
  failedGlyphs,
  finishedGlyphs,
  highlightRange,
  statementMarkers,
  useEditorMarks,
  type EditorLike,
  type MonacoLike,
} from './editorMarks';

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

  const PLAN = {
    plan_id: 'p1',
    stmts: [
      { index: 0, range: [0, 4], kind: 'read', verdict: 'allow', preview: 'ab' },
      { index: 1, range: [5, 7], kind: 'read', verdict: 'allow', preview: 'cd' },
    ],
    dialect: { engine: 'mysql' },
    connection_name: 'c',
    env_tag: null,
    read_only: false,
    tx_state: 'idle',
  } as unknown as PlanView;
  const failed = (position: number | null, over = {}): TabRun =>
    reduceRun(
      startRun(emptyRun, PLAN, 0),
      { t: 'statement_error', index: 1, error: appError('query', "Table 'x' doesn't exist", { position, ...over }) },
      1,
    );

  it('statement_without_position_gets_a_whole_statement_marker', () => {
    expect(statementMarkers(failed(null))).toEqual([
      { range: [5, 7], message: "Table 'x' doesn't exist", warning: false },
    ]);
    expect(statementMarkers(failed(6))).toEqual([]);
    expect(statementMarkers(failed(null, { outcome_unknown: true }))[0]!.warning).toBe(true);
  });

  it('whole_statement_marker_spans_the_statement', () => {
    const setModelMarkers = vi.fn();
    const monaco = { editor: { setModelMarkers }, MarkerSeverity: { Error: 8, Warning: 4 } } as unknown as MonacoLike;
    applyErrorMarkers(monaco, model as never, [], statementMarkers(failed(null)));
    expect(setModelMarkers.mock.calls[0]![2]).toEqual([
      { severity: 8, message: "Table 'x' doesn't exist", startLineNumber: 2, startColumn: 1, endLineNumber: 2, endColumn: 3 },
    ]);
  });

  it('failed_glyph_on_first_line_of_a_failed_statement', () => {
    const glyphs = failedGlyphs(model as never, PLAN, [1]);
    expect(glyphs).toHaveLength(1);
    expect(glyphs[0]!.range.startLineNumber).toBe(2);
    expect(glyphs[0]!.options.glyphMarginClassName).toBe('stmt-failed');
  });

  describe('useEditorMarks', () => {
    function fake() {
      let onContent: () => void = () => {};
      const sets: unknown[][] = [];
      const editor = {
        updateOptions: vi.fn(),
        createDecorationsCollection: () => ({ set: (d: unknown[]) => void sets.push(d), clear: vi.fn() }),
        getModel: () => model,
        getPosition: () => null,
        onDidChangeModelContent: (cb: () => void) => {
          onContent = cb;
          return { dispose: vi.fn() };
        },
        onDidChangeCursorPosition: () => ({ dispose: vi.fn() }),
      };
      const monaco = { editor: { setModelMarkers: vi.fn() }, MarkerSeverity: { Error: 8, Warning: 4 } };
      return { editor, monaco, sets, change: () => onContent() };
    }
    const finishedRun = (plan: PlanView, base: TabRun, id: number): TabRun =>
      reduceRun(
        reduceRun(startRun(base, plan, 5), { t: 'result_start', result_id: id, index: 0, columns: [] }, 6),
        { t: 'result_end', result_id: id, row_count: 0, affected: 1, truncated: false, elapsed_ms: 1 },
        7,
      );

    it('text_change_drops_the_glyphs_of_the_earlier_run', () => {
      resetStores();
      mockBackend({ statement_ranges: () => [] });
      const f = fake();
      const mounted = { editor: f.editor, monaco: f.monaco } as never;
      const done = finishedRun(PLAN, emptyRun, 1);
      const { rerender } = renderHook(({ run }) => useEditorMarks(mounted, 'mysql', run), { initialProps: { run: done } });
      expect(f.sets.at(-1)).toHaveLength(1);
      f.change();
      expect(f.sets.at(-1)).toEqual([]);
      // Later updates of the same run do not bring the stale dots back...
      rerender({ run: { ...done, finished: [0, 1] } });
      expect(f.sets.at(-1)).toEqual([]);
      // ...but the next run paints again.
      rerender({ run: finishedRun({ ...PLAN, plan_id: 'p2' }, done, 2) });
      expect(f.sets.at(-1)).toHaveLength(1);
    });

    it('current_statement_highlight_is_local_no_ipc_while_typing', () => {
      vi.useFakeTimers();
      try {
        resetStores();
        const invoke = mockBackend({});
        let text = 'SELECT 1;\nSELECT 2';
        let cursor = text.length;
        let onContent: (e?: { changes: { rangeOffset: number }[] }) => void = () => {};
        const highlights: unknown[][] = [];
        let collections = 0;
        const textModel = {
          getValue: () => text,
          getOffsetAt: () => cursor,
          getPositionAt: (o: number) => ({ lineNumber: text.slice(0, o).split('\n').length, column: o - text.lastIndexOf('\n', o - 1) }),
        };
        const editor = {
          updateOptions: vi.fn(),
          // The first collection is the highlight, the second the glyphs.
          createDecorationsCollection: () => {
            const mine = collections++ === 0;
            return { set: (d: unknown[]) => void (mine && highlights.push(d)), clear: vi.fn() };
          },
          getModel: () => textModel,
          getPosition: () => ({ lineNumber: 2, column: 1 }),
          onDidChangeModelContent: (cb: typeof onContent) => {
            onContent = cb;
            return { dispose: vi.fn() };
          },
          onDidChangeCursorPosition: () => ({ dispose: vi.fn() }),
        };
        const monaco = { editor: { setModelMarkers: vi.fn() }, MarkerSeverity: { Error: 8, Warning: 4 } };
        renderHook(() => useEditorMarks({ editor, monaco } as never, 'mysql', emptyRun));
        vi.advanceTimersByTime(200);
        // Second statement: line 2, columns 1..9.
        expect(highlights.at(-1)).toEqual([
          { range: { startLineNumber: 2, startColumn: 1, endLineNumber: 2, endColumn: 9 }, options: { className: 'current-stmt' } },
        ]);
        for (const ch of ' + 3 + 4') {
          const at = text.length;
          text += ch;
          cursor = text.length;
          onContent({ changes: [{ rangeOffset: at }] });
        }
        vi.advanceTimersByTime(200);
        expect(highlights.at(-1)).toEqual([
          { range: { startLineNumber: 2, startColumn: 1, endLineNumber: 2, endColumn: 17 }, options: { className: 'current-stmt' } },
        ]);
        expect(invoke).not.toHaveBeenCalled();
        // A change event without offsets (e.g. a flush): everything is split again.
        text = 'SELECT 12345;\nSELECT 2';
        cursor = 3;
        onContent({ changes: [] });
        vi.advanceTimersByTime(200);
        expect(highlights.at(-1)).toEqual([
          { range: { startLineNumber: 1, startColumn: 1, endLineNumber: 1, endColumn: 13 }, options: { className: 'current-stmt' } },
        ]);
      } finally {
        vi.useRealTimers();
      }
    });
  });
});
