import type { PlanNode } from '../../bindings';
import { fmtInt, fmtMs, fmtNum, fmtPct, iconOf, nodeTotals, rowsRatio, selfShare, type Insight } from './explainModel';

interface Props {
  node: PlanNode;
  costs: Map<PlanNode, number | null>;
  total: number | null;
  warnings: Insight[];
  /** The engine reports time and rows per loop (MySQL, PostgreSQL); else they are totals. */
  perLoop: boolean;
  /** The whole plan's time in total: the denominator of the time share. */
  planMs: number | null;
}

/** The inspector of the selected node: cost, rows, execution, the raw detail and warnings. */
export function ExplainDetails({ node, costs, total, warnings, perLoop, planMs }: Props) {
  // Per-loop figures only differ from the totals when the node ran more than once.
  const looped = perLoop && (node.loops ?? 1) > 1;
  const totals = nodeTotals(node, perLoop);
  const timeShare = totals.ms === null || planMs === null || planMs <= 0 ? null : totals.ms / planMs;
  const own = costs.get(node) ?? null;
  const share = selfShare(node, costs, total);
  const ratio = rowsRatio(node);
  const icon = iconOf(node.label);
  const detailLines = node.detail === null ? [] : node.detail.split(/;\s*/).filter((line) => line.length > 0);

  return (
    <aside className="ex-details" data-testid="explain-details" aria-label="Selected plan node">
      <div className="ex-details-head">
        <span className={`ex-ico ${icon.kind}`} aria-hidden="true">
          {icon.glyph}
        </span>
        <div className="ex-details-title">
          <h3>{node.label}</h3>
          {node.detail !== null && <p>{node.detail}</p>}
        </div>
      </div>
      <div className="ex-details-scroll">
        <section className="ex-section">
          <h4>Cost</h4>
          <div className="ex-kv">
            <span>Total cost</span>
            <b>{fmtNum(node.est_cost)}</b>
          </div>
          <div className="ex-kv">
            <span>Self cost</span>
            <b>{fmtNum(own)}</b>
          </div>
          <div className="ex-kv">
            <span>Share of self cost</span>
            <b>{fmtPct(share)}</b>
          </div>
        </section>

        <section className="ex-section">
          <h4>Rows</h4>
          <div className="ex-kv">
            <span>{looped ? 'Estimated, per loop' : 'Estimated'}</span>
            <b>{fmtInt(node.est_rows)}</b>
          </div>
          <div className="ex-kv">
            <span>{looped ? 'Actual, per loop' : 'Actual'}</span>
            <b>{fmtInt(node.actual_rows)}</b>
          </div>
          {looped && totals.rows !== null && (
            <div className="ex-kv">
              <span>Rows in total</span>
              <b>{fmtInt(totals.rows)}</b>
            </div>
          )}
          <div className="ex-kv">
            <span>Actual / estimated</span>
            <b>
              {ratio === null
                ? '—'
                : ratio >= 10
                  ? `${fmtInt(ratio)}×`
                  : `${Math.round(ratio * 100) / 100}×`}
            </b>
          </div>
        </section>

        <section className="ex-section">
          <h4>Execution</h4>
          {looped && (
            <div className="ex-kv">
              <span>Time per loop</span>
              <b>{fmtMs(node.actual_ms)}</b>
            </div>
          )}
          <div className="ex-kv">
            <span>Time in total</span>
            <b>
              {looped ? '≈ ' : ''}
              {fmtMs(totals.ms)}
              {timeShare !== null && <span className="ex-kv-sub"> · {fmtPct(timeShare)}</span>}
            </b>
          </div>
          <div className="ex-kv">
            <span>Loops</span>
            <b>{fmtInt(node.loops)}</b>
          </div>
          <div className="ex-kv">
            <span>Children</span>
            <b>{node.children.length}</b>
          </div>
        </section>

        {detailLines.length > 0 && (
          <section className="ex-section">
            <h4>Details</h4>
            {detailLines.map((line, i) => (
              <p className="ex-detail-line" key={i}>
                {line}
              </p>
            ))}
          </section>
        )}

        {warnings.length > 0 && (
          <section className="ex-section">
            <h4>Warnings</h4>
            {warnings.map((w, i) => (
              <div className="ex-warnbox" key={i} role="note">
                <span aria-hidden="true">⚠</span>
                <div>
                  <strong>{w.title}</strong>
                  <p>{w.body}</p>
                </div>
              </div>
            ))}
          </section>
        )}
      </div>
    </aside>
  );
}
