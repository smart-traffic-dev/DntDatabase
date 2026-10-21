import { useEffect, useMemo, useState } from 'react';
import type { PlanNode } from '../../bindings';
import { SearchIcon } from '../../components/icons';
import { emptyRun, useQueryRun } from '../../store/queryRun';
import { useQueryDocs } from '../../store/queryTabs';
import { explainQuery } from '../../store/runQuery';
import { showToast } from '../../store/toasts';
import { hottestNode, selfCosts } from './explainTree';
import {
  ancestorsOf,
  fmtMs,
  fmtNum,
  fmtPct,
  nodeTotals,
  planWarnings,
  reportsPerLoop,
  selfCostTotal,
  selfShare,
} from './explainModel';
import { ExplainDetails } from './ExplainDetails';
import { ExplainGrid } from './ExplainGrid';
import { AnalyzeButton, ExplainOverview } from './ExplainOverview';
import { PlanTreeView } from './PlanTreeView';
import { useRunGate } from './runGate';

type View = 'tree' | 'grid' | 'raw';

/**
 * The Explain panel of a query tab: the plan tree, the flat grid, the engine's raw output, and
 * the refusals. Selection, filter, collapse and the view live here; the views stay presentational.
 */
export function ExplainView({ tabId }: { tabId: string }) {
  const run = useQueryRun((s) => s.runs.get(tabId) ?? emptyRun);
  const connectionId = useQueryDocs((s) => s.docs.get(tabId)?.connectionId ?? null);
  const gate = useRunGate(tabId, connectionId);
  const perLoop = reportsPerLoop(gate.info);
  const plan = run.explain;
  const root = plan?.kind === 'tree' ? plan.root : null;
  const isTable = plan?.kind === 'table';

  const costs = useMemo(
    () => (root === null ? new Map<PlanNode, number | null>() : selfCosts(root)),
    [root],
  );
  const hot = useMemo(() => (root === null ? null : hottestNode(root, costs)), [root, costs]);
  const warnings = useMemo(() => (root === null ? new Map() : planWarnings(root)), [root]);
  const shareTotal = useMemo(() => (root === null ? null : selfCostTotal(costs)), [root, costs]);

  const [view, setView] = useState<View>('tree');
  const [selected, setSelected] = useState<PlanNode | null>(null);
  const [collapsed, setCollapsed] = useState<Set<PlanNode>>(new Set());
  const [filter, setFilter] = useState('');

  // A new plan starts clean: default view, the hottest node selected, everything expanded.
  useEffect(() => {
    setView(plan?.kind === 'table' ? 'grid' : 'tree');
    setSelected(null);
    setCollapsed(new Set());
    setFilter('');
  }, [plan]);

  const mode: View = isTable && view === 'tree' ? 'grid' : view;
  const current = selected !== null && costs.has(selected) ? selected : (hot ?? root);

  const select = (node: PlanNode) => {
    setSelected(node);
    if (root === null) return;
    // Reveal the node in the tree even when an ancestor is collapsed.
    setCollapsed((prev) => {
      if (prev.size === 0) return prev;
      const next = new Set(prev);
      for (const ancestor of ancestorsOf(root, node)) next.delete(ancestor);
      return next;
    });
  };

  const toggle = (node: PlanNode) => {
    setCollapsed((prev) => {
      const next = new Set(prev);
      if (next.has(node)) next.delete(node);
      else next.add(node);
      return next;
    });
  };

  const collapseAll = () => {
    if (root === null) return;
    const all = new Set<PlanNode>();
    const visit = (node: PlanNode) => {
      if (node.children.length > 0) all.add(node);
      node.children.forEach(visit);
    };
    visit(root);
    setCollapsed(all);
  };

  const copy = () => {
    if (!plan) return;
    void navigator.clipboard
      ?.writeText(plan.raw)
      .then(() => showToast({ kind: 'success', title: 'Plan copied' }))
      .catch(() => undefined);
  };

  const own = current === null ? null : (costs.get(current) ?? null);
  const share = current === null ? null : selfShare(current, costs, shareTotal);
  const planMs = root === null ? null : nodeTotals(root, perLoop).ms;
  const currentMs = current === null ? null : nodeTotals(current, perLoop).ms;
  const explain = (analyze: boolean) => void explainQuery(tabId, analyze);

  let body;
  if (run.explainError) {
    body = (
      <div className="qr-msg error" role="alert">
        {run.explainError}
      </div>
    );
  } else if (!plan) {
    body = (
      <div className="qr-empty ex-start">
        <p>No plan yet for the statement at the cursor.</p>
        <div className="ex-start-actions">
          <button
            type="button"
            className="ex-btn primary"
            disabled={!gate.runnable}
            title={gate.runnable ? 'Explain (Ctrl+Shift+E)' : gate.why}
            onClick={() => explain(false)}
          >
            Explain <kbd>Ctrl+Shift+E</kbd>
          </button>
          <AnalyzeButton gate={gate} onAnalyze={() => explain(true)} label="Explain Analyze" />
        </div>
        <p className="ex-start-note">Analyze runs the statement; on Production it asks first.</p>
      </div>
    );
  } else if (mode === 'raw') {
    body = (
      <pre className="ex-raw" data-testid="explain-raw">
        {plan.raw}
      </pre>
    );
  } else if (plan.kind === 'table') {
    body = (
      <ExplainGrid
        plan={plan}
        costs={costs}
        warnings={warnings}
        hot={hot}
        selected={null}
        filter={filter}
        onSelect={() => undefined}
      />
    );
  } else {
    body = (
      <div className="ex-main">
        <div className="ex-pane">
          {mode === 'tree' ? (
            <PlanTreeView
              root={plan.root}
              costs={costs}
              warnings={warnings}
              hot={hot}
              selected={current}
              collapsed={collapsed}
              filter={filter}
              total={shareTotal}
              perLoop={perLoop}
              onToggle={toggle}
              onSelect={select}
            />
          ) : (
            <ExplainGrid
              plan={plan}
              costs={costs}
              warnings={warnings}
              hot={hot}
              selected={current}
              filter={filter}
              onSelect={select}
            />
          )}
        </div>
        {current !== null && (
          <ExplainDetails
            node={current}
            costs={costs}
            total={shareTotal}
            warnings={warnings.get(current) ?? []}
            perLoop={perLoop}
            planMs={planMs}
          />
        )}
      </div>
    );
  }

  return (
    <div className="ex-view" data-testid="explain-view">
      {plan && !run.explainError && (
        <div className="ex-bar" role="toolbar" aria-label="Explain controls">
          <div className="ex-views" role="group" aria-label="Plan view">
            {!isTable && (
              <button
                type="button"
                className="ex-view-btn"
                aria-pressed={mode === 'tree'}
                onClick={() => setView('tree')}
              >
                Tree
              </button>
            )}
            <button
              type="button"
              className="ex-view-btn"
              aria-pressed={mode === 'grid'}
              onClick={() => setView('grid')}
            >
              Grid
            </button>
            <button
              type="button"
              className="ex-view-btn"
              aria-pressed={mode === 'raw'}
              onClick={() => setView(mode === 'raw' ? (isTable ? 'grid' : 'tree') : 'raw')}
            >
              Raw
            </button>
          </div>

          {mode !== 'raw' && (
            <div className="ex-search">
              <SearchIcon />
              <input
                type="text"
                value={filter}
                placeholder="Filter nodes…"
                aria-label="Filter plan nodes"
                onChange={(e) => setFilter(e.target.value)}
              />
              {filter.length > 0 && (
                <button
                  type="button"
                  className="ex-search-clear"
                  aria-label="Clear filter"
                  onClick={() => setFilter('')}
                >
                  ×
                </button>
              )}
            </div>
          )}

          {mode === 'tree' && !isTable && (
            <>
              <button
                type="button"
                className="ex-btn"
                disabled={filter.trim().length > 0}
                title={filter.trim().length > 0 ? 'Clear the filter to change the tree' : undefined}
                onClick={() => setCollapsed(new Set())}
              >
                Expand all
              </button>
              <button
                type="button"
                className="ex-btn"
                disabled={filter.trim().length > 0}
                title={filter.trim().length > 0 ? 'Clear the filter to change the tree' : undefined}
                onClick={collapseAll}
              >
                Collapse all
              </button>
            </>
          )}

          <span className="ex-bar-spacer" />
          <button type="button" className="ex-btn" onClick={copy}>
            Copy plan
          </button>
        </div>
      )}

      {root !== null && mode !== 'raw' && (
        <ExplainOverview
          root={root}
          costs={costs}
          warnings={warnings}
          hot={hot}
          selected={current}
          gate={gate}
          onSelect={select}
          onAnalyze={() => explain(true)}
        />
      )}

      <div className="ex-body">{body}</div>

      {root !== null && mode !== 'raw' && current !== null && (
        <div className="ex-foot" data-testid="explain-foot">
          <span className="ex-foot-label">{current.label}</span>
          <span>
            self {fmtNum(own)} ({fmtPct(share)})
          </span>
          {currentMs !== null && <span>{fmtMs(currentMs)}</span>}
        </div>
      )}
    </div>
  );
}
