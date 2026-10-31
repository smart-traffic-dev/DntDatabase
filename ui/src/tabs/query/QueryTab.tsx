import { memo, useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react';
import { useShallow } from 'zustand/react/shallow';
import { Splitter } from '../../components/Splitter';
import { useConnections } from '../../store/connections';
import { EDITOR_MIN_PX, EDITOR_RATIO_DEFAULT, RESULT_MIN_PX, editorPixels, resultsOf, useLayout } from '../../store/layout';
import { emptyRun, useQueryRun, type TabRun } from '../../store/queryRun';
import { useQueryDocs } from '../../store/queryTabs';
import { useTabs, type QueryTabRef } from '../../store/tabs';
import { sqlEngine, type Engine } from '../../query/engine';
import { useEditorMarks, type EditorLike, type MonacoLike } from './editorMarks';
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
 * A query tab: the editor, results and messages below (its controls are in the tab strip). Until
 * the tab first runs there is no results panel; after that the user can fold it to its tab bar
 * (choosing one of its tabs, or a new run, opens it again).
 */
export function QueryTab({ tab }: { tab: QueryTabRef }) {
  // Field selectors only (not the document): the text changes on every keystroke. The toolbar
  // is in the group's tab strip (TabStripActions).
  const toolbarDoc = useQueryDocs(
    useShallow((s) => {
      const d = s.docs.get(tab.id);
      return d ? { connectionId: d.connectionId, dialect: d.dialect } : null;
    }),
  );
  const connectionId = toolbarDoc?.connectionId ?? null;
  const kind = useConnections((s) => (connectionId ? s.summaries.get(connectionId)?.config.kind : undefined));
  const run = useQueryRun((s) => s.runs.get(tab.id) ?? emptyRun);
  const [mounted, setMounted] = useState<{ editor: EditorLike; monaco: MonacoLike } | null>(null);
  const onMount = useCallback<NonNullable<SqlEditorProps['onMount']>>((editor, monaco) => setMounted({ editor, monaco }), []);
  const engine = sqlEngine(kind) ?? toolbarDoc?.dialect ?? 'mysql';
  useEditorMarks(mounted, engine, run);
  // The editor / results split: a fraction of the measured body height (not subscribed to the text).
  // Visibility, split and maximize belong to the editor group the tab sits in.
  const groupId = useTabs((s) => s.groupOf[tab.id] ?? s.focusedGroupId);
  const results = useLayout(useShallow((s) => resultsOf(s, groupId)));
  const { ratio, maximized } = results;
  const resultsVisible = results.visible;
  const setGroupRatio = useLayout((s) => s.setGroupRatio);
  const toggleGroupMaximize = useLayout((s) => s.toggleGroupMaximize);
  // Folded to its tab bar by the user; a new run opens it again to show its outcome.
  const [folded, setFolded] = useState(false);
  useEffect(() => {
    if (run.runId !== null) setFolded(false);
  }, [run.runId]);
  const onOpenResults = useCallback(() => setFolded(false), []);
  const onToggleCollapse = useCallback(() => setFolded((f) => !f), []);
  const onToggleMaximize = useCallback(() => toggleGroupMaximize(groupId), [groupId, toggleGroupMaximize]);
  // Until the tab first runs there is nothing to show: no results panel at all (maximizing shows it).
  const hasResults = ranOnce(run) || maximized;
  const collapsed = folded && !maximized;
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
      <div className="query-split" ref={splitRef}>
        <div
          className="query-editor-pane"
          hidden={maximized && resultsVisible}
          style={{
            flex: !resultsVisible || !hasResults || collapsed ? '1 1 0' : editor === null ? `0 0 ${ratio * 100}%` : `0 0 ${editor}px`,
          }}
        >
          <EditorHost tabId={tab.id} engine={engine} onMount={onMount} />
        </div>
        {resultsVisible && hasResults && collapsed && (
          <ResultPanel
            tabId={tab.id}
            collapsed
            onOpen={onOpenResults}
            onToggleCollapse={onToggleCollapse}
            maximized={false}
            onToggleMaximize={onToggleMaximize}
          />
        )}
        {resultsVisible && hasResults && !collapsed && (
          <>
            {!maximized && (
            <Splitter
              orientation="horizontal"
              label="Resize editor and results"
              value={editor ?? 0}
              min={EDITOR_MIN_PX}
              max={Math.max(EDITOR_MIN_PX, total - RESULT_MIN_PX)}
              defaultValue={editorPixels(EDITOR_RATIO_DEFAULT, total)}
              onChange={(px) => total > 0 && setGroupRatio(groupId, px / total)}
            />
            )}
            <ResultPanel
              tabId={tab.id}
              maximized={maximized}
              onToggleMaximize={onToggleMaximize}
              onToggleCollapse={maximized ? undefined : onToggleCollapse}
            />
          </>
        )}
      </div>
    </div>
  );
}
