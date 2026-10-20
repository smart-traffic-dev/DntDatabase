import { useMemo, useState } from 'react';
import type { PlanNode } from '../../bindings';
import { emptyRun, useQueryRun } from '../../store/queryRun';
import { hottestNode, selfCosts } from './explainTree';

type Costs = Map<PlanNode, number | null>;

// Rounded first, so float noise (0.9 - 0.55) does not show as 0.35000000000000003.
const fmt = (n: number) => {
  const r = Math.round(n * 100) / 100;
  return Number.isInteger(r) ? String(r) : r.toFixed(2);
};

function Metrics({ node, costs }: { node: PlanNode; costs: Costs }) {
  const parts: string[] = [];
  const own = costs.get(node) ?? null;
  if (node.est_cost !== null) parts.push(`cost ${fmt(node.est_cost)}`);
  if (own !== null) parts.push(`self ${fmt(own)}`);
  if (node.est_rows !== null) parts.push(`rows ${fmt(node.est_rows)}`);
  if (node.actual_rows !== null) parts.push(`actual rows ${fmt(node.actual_rows)}`);
  if (node.actual_ms !== null) parts.push(`${fmt(node.actual_ms)} ms`);
  if (node.loops !== null) parts.push(`loops ${fmt(node.loops)}`);
  return parts.length > 0 ? <span className="ex-metrics">{parts.join(' · ')}</span> : null;
}

function TreeNode({ node, hot, costs }: { node: PlanNode; hot: PlanNode | null; costs: Costs }) {
  const [open, setOpen] = useState(true);
  const hasChildren = node.children.length > 0;
  return (
    <li>
      <div className={`ex-node${node === hot ? ' hot' : ''}`} data-testid="explain-node">
        {hasChildren ? (
          <button
            type="button"
            className="ex-toggle"
            aria-expanded={open}
            aria-label={open ? `Collapse ${node.label}` : `Expand ${node.label}`}
            onClick={() => setOpen((v) => !v)}
          >
            {open ? '▾' : '▸'}
          </button>
        ) : (
          <span className="ex-toggle" aria-hidden="true" />
        )}
        <span className="ex-label">{node.label}</span>
        {node.detail && <span className="ex-detail">{node.detail}</span>}
        <Metrics node={node} costs={costs} />
      </div>
      {hasChildren && open && (
        <ul>
          {node.children.map((c, i) => (
            <TreeNode key={i} node={c} hot={hot} costs={costs} />
          ))}
        </ul>
      )}
    </li>
  );
}

/** The plan tree, its own costs computed once per plan. */
function PlanTreeView({ root }: { root: PlanNode }) {
  const { costs, hot } = useMemo(() => {
    const costs = selfCosts(root);
    return { costs, hot: hottestNode(root, costs) };
  }, [root]);
  return (
    <ul className="ex-tree">
      <TreeNode node={root} hot={hot} costs={costs} />
    </ul>
  );
}

/** The Explain panel of a query tab: plan tree (or table), "Raw" output, and refusals. */
export function ExplainView({ tabId }: { tabId: string }) {
  const run = useQueryRun((s) => s.runs.get(tabId) ?? emptyRun);
  const [raw, setRaw] = useState(false);
  const plan = run.explain;

  let body;
  if (run.explainError) {
    body = (
      <div className="qr-msg error" role="alert">
        {run.explainError}
      </div>
    );
  } else if (!plan) {
    body = <div className="qr-empty">No plan yet. Run Explain (Ctrl+Shift+E) on the statement at the cursor.</div>;
  } else if (raw) {
    body = (
      <pre className="ex-raw" data-testid="explain-raw">
        {plan.raw}
      </pre>
    );
  } else if (plan.kind === 'table') {
    body = (
      <table className="ex-table" data-testid="explain-table">
        <thead>
          <tr>
            {plan.columns.map((c, i) => (
              <th key={i}>{c}</th>
            ))}
          </tr>
        </thead>
        <tbody>
          {plan.rows.map((r, i) => (
            <tr key={i}>
              {r.map((v, j) => (
                <td key={j}>{v}</td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    );
  } else {
    body = <PlanTreeView root={plan.root} />;
  }

  return (
    <div className="ex-view" data-testid="explain-view">
      {plan && !run.explainError && (
        <div className="ex-bar">
          <button type="button" className="btn small" aria-pressed={raw} onClick={() => setRaw((v) => !v)}>
            Raw
          </button>
        </div>
      )}
      <div className="ex-body">{body}</div>
    </div>
  );
}
