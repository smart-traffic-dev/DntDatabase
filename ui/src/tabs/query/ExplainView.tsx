import { useState } from 'react';
import type { PlanNode } from '../../bindings';
import { emptyRun, useQueryRun } from '../../store/queryRun';
import { hottestNode, selfCost } from './explainTree';

const fmt = (n: number) => (Number.isInteger(n) ? String(n) : n.toFixed(2));

function Metrics({ node }: { node: PlanNode }) {
  const parts: string[] = [];
  const own = selfCost(node);
  if (node.est_cost !== null) parts.push(`cost ${fmt(node.est_cost)}`);
  if (own !== null) parts.push(`self ${fmt(own)}`);
  if (node.est_rows !== null) parts.push(`rows ${fmt(node.est_rows)}`);
  if (node.actual_rows !== null) parts.push(`actual rows ${fmt(node.actual_rows)}`);
  if (node.actual_ms !== null) parts.push(`${fmt(node.actual_ms)} ms`);
  if (node.loops !== null) parts.push(`loops ${fmt(node.loops)}`);
  return parts.length > 0 ? <span className="ex-metrics">{parts.join(' · ')}</span> : null;
}

function TreeNode({ node, hot }: { node: PlanNode; hot: PlanNode | null }) {
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
        <Metrics node={node} />
      </div>
      {hasChildren && open && (
        <ul>
          {node.children.map((c, i) => (
            <TreeNode key={i} node={c} hot={hot} />
          ))}
        </ul>
      )}
    </li>
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
    const hot = hottestNode(plan.root);
    body = (
      <ul className="ex-tree">
        <TreeNode node={plan.root} hot={hot} />
      </ul>
    );
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
