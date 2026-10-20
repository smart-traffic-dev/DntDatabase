// Editor decorations of a query tab (spec §6.2, §9): the current statement, error markers and
// the gutter glyph of finished statements. Every offset is a UTF-16 offset into the editor
// text, which is what Monaco's model uses.
import type * as Monaco from 'monaco-editor';
import { useEffect, useRef } from 'react';
import { api } from '../../api';
import type { PlanView } from '../../bindings';
import { engineDialect, type Engine } from '../../store/queryTabs';
import type { TabRun } from '../../store/queryRun';
import { currentStatement } from './format';

export type MonacoLike = typeof Monaco;
export type EditorLike = Monaco.editor.IStandaloneCodeEditor;
type Model = Monaco.editor.ITextModel;

export const MARKER_OWNER = 'dnt-query';
export const HIGHLIGHT_DEBOUNCE_MS = 150;

type Rng = { startLineNumber: number; startColumn: number; endLineNumber: number; endColumn: number };

export function highlightRange(model: Model, [from, to]: [number, number]): Rng {
  const a = model.getPositionAt(from);
  const b = model.getPositionAt(to);
  return { startLineNumber: a.lineNumber, startColumn: a.column, endLineNumber: b.lineNumber, endColumn: b.column };
}

/** Error markers at the offsets the backend mapped (`AppError.position`); replaces the previous ones. */
export function applyErrorMarkers(monaco: MonacoLike, model: Model, markers: TabRun['markers']) {
  const data = markers.map((m) => {
    const at = Math.min(m.offset, model.getValueLength());
    const start = model.getPositionAt(at);
    const word = model.getWordAtPosition(start);
    const endColumn = word && word.endColumn > start.column ? word.endColumn : start.column + 1;
    return {
      severity: monaco.MarkerSeverity.Error,
      message: m.message,
      startLineNumber: start.lineNumber,
      startColumn: start.column,
      endLineNumber: start.lineNumber,
      endColumn,
    };
  });
  monaco.editor.setModelMarkers(model, MARKER_OWNER, data);
}

/** One gutter glyph on the first line of each finished statement. */
export function finishedGlyphs(model: Model, plan: PlanView, finished: number[]) {
  return finished.flatMap((i) => {
    const stmt = plan.stmts.find((s) => s.index === i);
    if (!stmt) return [];
    const start = model.getPositionAt(Math.min(stmt.range[0], model.getValueLength()));
    return [
      {
        range: { startLineNumber: start.lineNumber, startColumn: 1, endLineNumber: start.lineNumber, endColumn: 1 },
        options: { glyphMarginClassName: 'stmt-done', glyphMarginHoverMessage: { value: 'Finished' } },
      },
    ];
  });
}

interface Mounted {
  editor: EditorLike;
  monaco: MonacoLike;
}

/** Keeps the highlight, markers and glyphs of `mounted` in step with the document and its run. */
export function useEditorMarks(mounted: Mounted | null, engine: Engine, run: TabRun) {
  const ranges = useRef<[number, number][]>([]);
  const highlight = useRef<Monaco.editor.IEditorDecorationsCollection | null>(null);
  const glyphs = useRef<Monaco.editor.IEditorDecorationsCollection | null>(null);

  // Current statement: statement_ranges (debounced) + the cursor.
  useEffect(() => {
    if (!mounted) return;
    const { editor } = mounted;
    editor.updateOptions({ glyphMargin: true });
    highlight.current ??= editor.createDecorationsCollection();
    glyphs.current ??= editor.createDecorationsCollection();
    let timer: ReturnType<typeof setTimeout> | undefined;
    let alive = true;
    const paint = () => {
      const model = editor.getModel();
      const pos = editor.getPosition();
      if (!model || !pos) return;
      const cur = currentStatement(ranges.current, model.getOffsetAt(pos));
      highlight.current?.set(
        cur ? [{ range: highlightRange(model, cur), options: { className: 'current-stmt' } }] : [],
      );
    };
    const refresh = () => {
      clearTimeout(timer);
      timer = setTimeout(() => {
        const sql = editor.getModel()?.getValue() ?? '';
        api
          .statementRanges(sql, engineDialect(engine))
          .then((r) => {
            if (!alive) return;
            ranges.current = r;
            paint();
          })
          .catch(() => {});
      }, HIGHLIGHT_DEBOUNCE_MS);
    };
    refresh();
    const subs = [editor.onDidChangeModelContent(refresh), editor.onDidChangeCursorPosition(paint)];
    return () => {
      alive = false;
      clearTimeout(timer);
      subs.forEach((s) => s.dispose());
    };
  }, [mounted, engine]);

  // Error markers and finished-statement glyphs follow the run.
  useEffect(() => {
    const model = mounted?.editor.getModel();
    if (!mounted || !model) return;
    applyErrorMarkers(mounted.monaco, model, run.markers);
    glyphs.current?.set(run.plan ? finishedGlyphs(model, run.plan, run.finished) : []);
  }, [mounted, run.markers, run.plan, run.finished]);
}
