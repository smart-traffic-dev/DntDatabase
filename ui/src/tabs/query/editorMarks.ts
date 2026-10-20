// Editor decorations of a query tab (spec §6.2, §9): the current statement, error markers and
// the gutter glyphs of finished / failed statements. Every offset is a UTF-16 offset into the
// editor text, which is what Monaco's model uses.
import type * as Monaco from 'monaco-editor';
import { useEffect, useMemo, useRef } from 'react';
import type { PlanView } from '../../bindings';
import { splitIncremental, type SplitState } from '../../query/completion/split';
import { stmtsByIndex } from '../../query/plan';
import type { Engine } from '../../query/engine';
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

/** A failed statement the backend gave no position for: the whole statement gets the marker (F45). */
export interface StatementMarker {
  range: [number, number];
  message: string;
  /** Outcome unknown (may have been applied): a warning rather than an error. */
  warning: boolean;
}

/** Statements that failed (a plain user cancel is not one), with the tone of their Messages line. */
function failures(run: Pick<TabRun, 'messages'>) {
  return run.messages.flatMap((m) =>
    m.index !== null && m.error !== null && m.tone !== 'info'
      ? [{ index: m.index, error: m.error, warning: m.tone === 'warning' }]
      : [],
  );
}

/** Whole-statement markers of failed statements whose error has no editor position. */
export function statementMarkers(run: Pick<TabRun, 'plan' | 'messages'>): StatementMarker[] {
  if (!run.plan) return [];
  const byIndex = stmtsByIndex(run.plan);
  return failures(run).flatMap((f) => {
    const stmt = byIndex.get(f.index);
    if (!stmt || f.error.position !== null) return [];
    return [{ range: [stmt.range[0], stmt.range[1]] as [number, number], message: f.error.message, warning: f.warning }];
  });
}

/** Indexes of the statements that failed in the run. */
export const failedIndexes = (run: Pick<TabRun, 'messages'>): number[] => [
  ...new Set(failures(run).map((f) => f.index)),
];

/**
 * Error markers at the offsets the backend mapped (`AppError.position`) and over whole
 * statements for errors without one; replaces the previous ones.
 */
export function applyErrorMarkers(
  monaco: MonacoLike,
  model: Model,
  markers: TabRun['markers'],
  statements: StatementMarker[] = [],
) {
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
  for (const s of statements) {
    const a = model.getPositionAt(Math.min(s.range[0], model.getValueLength()));
    const b = model.getPositionAt(Math.min(s.range[1], model.getValueLength()));
    data.push({
      severity: s.warning ? monaco.MarkerSeverity.Warning : monaco.MarkerSeverity.Error,
      message: s.message,
      startLineNumber: a.lineNumber,
      startColumn: a.column,
      endLineNumber: b.lineNumber,
      endColumn: b.column,
    });
  }
  monaco.editor.setModelMarkers(model, MARKER_OWNER, data);
}

function glyphsOf(model: Model, plan: PlanView, indexes: number[], className: string, hover: string) {
  const byIndex = stmtsByIndex(plan);
  return indexes.flatMap((i) => {
    const stmt = byIndex.get(i);
    if (!stmt) return [];
    const start = model.getPositionAt(Math.min(stmt.range[0], model.getValueLength()));
    return [
      {
        range: { startLineNumber: start.lineNumber, startColumn: 1, endLineNumber: start.lineNumber, endColumn: 1 },
        options: { glyphMarginClassName: className, glyphMarginHoverMessage: { value: hover } },
      },
    ];
  });
}

/** One gutter glyph on the first line of each finished statement. */
export const finishedGlyphs = (model: Model, plan: PlanView, finished: number[]) =>
  glyphsOf(model, plan, finished, 'stmt-done', 'Finished');

/** One red gutter glyph on the first line of each failed statement (F45). */
export const failedGlyphs = (model: Model, plan: PlanView, failed: number[]) =>
  glyphsOf(model, plan, failed, 'stmt-failed', 'Failed');

interface Mounted {
  editor: EditorLike;
  monaco: MonacoLike;
}

/** Keeps the highlight, markers and glyphs of `mounted` in step with the document and its run. */
export function useEditorMarks(mounted: Mounted | null, engine: Engine, run: TabRun) {
  const ranges = useRef<[number, number][]>([]);
  const highlight = useRef<Monaco.editor.IEditorDecorationsCollection | null>(null);
  const glyphs = useRef<Monaco.editor.IEditorDecorationsCollection | null>(null);
  // The plan the editor text changed under: its offsets, markers and dots no longer describe the text.
  const staleFor = useRef<string | null>(null);
  const planId = useRef<string | null>(null);
  planId.current = run.plan?.plan_id ?? null;

  // Current statement: the local splitter (debounced, no IPC while typing) + the cursor.
  // Display only: what runs is still cut by the backend when the run starts.
  useEffect(() => {
    if (!mounted) return;
    const { editor } = mounted;
    editor.updateOptions({ glyphMargin: true });
    highlight.current ??= editor.createDecorationsCollection();
    glyphs.current ??= editor.createDecorationsCollection();
    let timer: ReturnType<typeof setTimeout> | undefined;
    // Local split (UI only, see split.ts), redone from the statement before the first edit.
    let split: SplitState | null = null;
    let changedFrom = 0;
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
        split = splitIncremental(split, sql, engine, changedFrom);
        changedFrom = Infinity;
        ranges.current = split.ranges;
        paint();
      }, HIGHLIGHT_DEBOUNCE_MS);
    };
    refresh();
    const subs = [
      editor.onDidChangeModelContent((e) => {
        // No change list (or a flush): split everything again.
        const offsets = e?.changes?.map((c) => c.rangeOffset) ?? [0];
        changedFrom = Math.min(changedFrom, ...offsets);
        // Markers and "done" dots of the last run describe text that has changed (F08).
        const model = editor.getModel();
        if (model) mounted.monaco.editor.setModelMarkers(model, MARKER_OWNER, []);
        staleFor.current = planId.current;
        glyphs.current?.set([]);
        refresh();
      }),
      editor.onDidChangeCursorPosition(paint),
    ];
    return () => {
      highlight.current?.clear();
      glyphs.current?.clear();
      highlight.current = null;
      glyphs.current = null;
      clearTimeout(timer);
      subs.forEach((s) => s.dispose());
    };
  }, [mounted, engine]);

  // Error markers and finished / failed statement glyphs follow the run.
  const statements = useMemo(() => statementMarkers({ plan: run.plan, messages: run.messages }), [run.plan, run.messages]);
  const failed = useMemo(() => failedIndexes({ messages: run.messages }), [run.messages]);
  useEffect(() => {
    const model = mounted?.editor.getModel();
    if (!mounted || !model) return;
    if (run.plan && staleFor.current === run.plan.plan_id) return;
    applyErrorMarkers(mounted.monaco, model, run.markers, statements);
    glyphs.current?.set(
      run.plan ? [...finishedGlyphs(model, run.plan, run.finished), ...failedGlyphs(model, run.plan, failed)] : [],
    );
  }, [mounted, run.markers, run.plan, run.finished, statements, failed]);
}
