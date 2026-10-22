import type { PlanNode } from '../../bindings';
import { allWarnings, fmtInt, fmtMs, fmtNum, fmtPct, nodeCount, selfCostTotal, selfShare, totalCost, type Insight } from './explainModel';
import { ANALYZE_NEEDS, type RunGate } from './runGate';

interface Props {
  root: PlanNode;
  costs: Map<PlanNode, number | null>;
  warnings: Map<PlanNode, Insight[]>;
  hot: PlanNode | null;
  gate: RunGate;
  onSelect(node: PlanNode): void;
  onAnalyze(): void;
}

/** Explain Analyze from the panel: disabled while the tab cannot run or the server lacks it. */
export function AnalyzeButton({ gate, onAnalyze, label }: { gate: RunGate; onAnalyze(): void; label: string }) {
  const enabled = gate.runnable && gate.analyzeOk;
  return (
    <button
      type="button"
      className="ex-btn"
      disabled={!enabled}
      title={!gate.runnable ? gate.why : gate.analyzeOk ? 'Runs the statement and reports real timings' : ANALYZE_NEEDS}
      onClick={onAnalyze}
    >
      {label}
    </button>
  );
}

/** The one-line overview above the plan: totals, then the hottest node (a click selects it). */
export function ExplainOverview({ root, costs, warnings, hot, gate, onSelect, onAnalyze }: Props) {
  const total = totalCost(root, costs);
  const shareTotal = selfCostTotal(costs);
  const warnList = allWarnings(warnings);
  const analyzed = root.actual_rows !== null || root.actual_ms !== null;
  const rows = analyzed ? root.actual_rows : root.est_rows;
  const hotShare = hot ? selfShare(hot, costs, shareTotal) : null;

  return (
    <div className="ex-overview" data-testid="explain-overview">
      <div className="ex-stats">
        <div className="ex-stat">
          <span>Total cost</span>
          <b>{fmtNum(total)}</b>
        </div>
        <div className="ex-stat">
          <span>{analyzed ? 'Rows' : 'Est. rows'}</span>
          <b>{fmtInt(rows)}</b>
        </div>
        {root.actual_ms !== null && (
          <div className="ex-stat">
            <span>Actual time</span>
            <b>{fmtMs(root.actual_ms)}</b>
          </div>
        )}
        <div className="ex-stat">
          <span>Nodes</span>
          <b>{nodeCount(root)}</b>
        </div>
        {warnList.length > 0 && (
          <div className="ex-stat warn" title={warnList.map((w) => w.title).join('\n')}>
            <span>Warnings</span>
            <b role="img" aria-label={`${warnList.length} warning${warnList.length === 1 ? '' : 's'}`}>
              ⚠ {warnList.length}
            </b>
          </div>
        )}
        {!analyzed && (
          <>
            <span className="ex-tag" title="Estimates only: run Explain Analyze for actual rows and time.">
              Not analyzed
            </span>
            <AnalyzeButton gate={gate} onAnalyze={onAnalyze} label="Run Explain Analyze" />
          </>
        )}
      </div>

      <span className="ex-bar-spacer" />

      {hot !== null && (
        <button
          type="button"
          className="ex-hot"
          onClick={() => onSelect(hot)}
          title="Hottest node — click to select"
        >
          <span className="ex-hot-tag">HOT</span>
          <b>{hot.label}</b>
          <span>{fmtPct(hotShare)}</span>
        </button>
      )}
    </div>
  );
}
