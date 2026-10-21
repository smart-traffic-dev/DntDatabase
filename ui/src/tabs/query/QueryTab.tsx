import { memo, useCallback, useLayoutEffect, useRef, useState } from 'react';
import { useShallow } from 'zustand/react/shallow';
import { Splitter } from '../../components/Splitter';
import { useConnections } from '../../store/connections';
import { EDITOR_MIN_PX, EDITOR_RATIO_DEFAULT, RESULT_MIN_PX, editorPixels, useLayout } from '../../store/layout';
import { emptyRun, useQueryRun, type TabRun } from '../../store/queryRun';
import { useQueryDocs } from '../../store/queryTabs';
import type { QueryTabRef } from '../../store/tabs';
import type { Engine } from '../../query/engine';
import { useEditorMarks, type EditorLike, type MonacoLike } from './editorMarks';
import { QueryToolbar } from './QueryToolbar';
import { ResultPanel } from './ResultPanel';
import { SqlEditor, type SqlEditorProps } from './SqlEditor';
import './query.css';

/**
 * The editor and its document. The only part of a query tab that subscribes to the text:
 * a keystroke re-renders this, not the toolbar or the result panel.
 */
const EditorHost = memo(function EditorHost({
  tabId,
  engine,
  onMount,
}: {
  tabId: string;
  engine: Engine;
  onMount: SqlEditorProps['onMount'];
}) {
  const doc = useQueryDocs((s) => s.docs.get(tabId));
  return doc ? <SqlEditor doc={doc} engine={engine} onMount={onMount} /> : null;
});

/** The tab has run (or explained) something: its results open from then on. */
const ranOnce = (run: TabRun) =>
  run.runId !== null || run.phase.kind !== 'idle' || run.explain !== null || run.explainError !== null || run.messages.length > 0;

/**
 * A query tab: toolbar over the editor, results and messages below. Until the tab first runs,
 * the results are collapsed to their tab bar (which shows the run keys); clicking one of its
 * tabs opens them too.
 */
export function QueryTab({ tab }: { tab: QueryTabRef }) {
  // Field selectors only (not the document): the text changes on every keystroke.
  const toolbarDoc = useQueryDocs(
    useShallow((s) => {
      const d = s.docs.get(tab.id);
      return d
        ? {
            id: d.id,
            connectionId: d.connectionId,
            database: d.database,
            rowLimit: d.rowLimit,
            continueOnError: d.continueOnError,
            dialect: d.dialect,
          }
        : null;
    }),
  );
  const connectionId = toolbarDoc?.connectionId ?? null;
  const kind = useConnections((s) => (connectionId ? s.summaries.get(connectionId)?.config.kind : undefined));
  const run = useQueryRun((s) => s.runs.get(tab.id) ?? emptyRun);
  const [mounted, setMounted] = useState<{ editor: EditorLike; monaco: MonacoLike } | null>(null);
  const onMount = useCallback<NonNullable<SqlEditorProps['onMount']>>((editor, monaco) => setMounted({ editor, monaco }), []);
  const engine = kind ?? toolbarDoc?.dialect ?? 'mysql';
  useEditorMarks(mounted, engine, run);
  // The editor / results split: a fraction of the measured body height (not subscribed to the text).
  const ratio = useLayout((s) => s.editorRatio);
  const setEditorRatio = useLayout((s) => s.setEditorRatio);
  const resultsVisible = useLayout((s) => s.resultsVisible);
  const [opened, setOpened] = useState(false);
  const onOpenResults = useCallback(() => setOpened(true), []);
  const collapsed = !ranOnce(run) && !opened;
  const splitRef = useRef<HTMLDivElement>(null);
  const [total, setTotal] = useState(0);
  const missing = !toolbarDoc;
  useLayoutEffect(() => {
    const el = splitRef.current;
    if (!el) return;
    setTotal(el.clientHeight);
    if (typeof ResizeObserver === 'undefined') return;
    const observer = new ResizeObserver(() => setTotal(el.clientHeight));
    observer.observe(el);
    return () => observer.disconnect();
  }, [missing]);
  const editor = total > 0 ? editorPixels(ratio, total) : null;
  if (!toolbarDoc) {
    return (
      <div className="empty">
        <strong>Query not found</strong>
      </div>
    );
  }
  return (
    <div className="query-tab">
      <QueryToolbar doc={toolbarDoc} />
      <div className="query-split" ref={splitRef}>
        <div
          className="query-editor-pane"
          style={{ flex: !resultsVisible || collapsed ? '1 1 0' : editor === null ? `0 0 ${ratio * 100}%` : `0 0 ${editor}px` }}
        >
          <EditorHost tabId={tab.id} engine={engine} onMount={onMount} />
        </div>
        {resultsVisible && collapsed && <ResultPanel tabId={tab.id} collapsed onOpen={onOpenResults} />}
        {resultsVisible && !collapsed && (
          <>
            <Splitter
              orientation="horizontal"
              label="Resize editor and results"
              value={editor ?? 0}
              min={EDITOR_MIN_PX}
              max={Math.max(EDITOR_MIN_PX, total - RESULT_MIN_PX)}
              defaultValue={editorPixels(EDITOR_RATIO_DEFAULT, total)}
              onChange={(px) => total > 0 && setEditorRatio(px / total)}
            />
            <ResultPanel tabId={tab.id} />
          </>
        )}
      </div>
    </div>
  );
}
