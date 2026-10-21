import { useMemo } from 'react';
import type { PlanNode } from '../../bindings';
import { allWarnings, costSegments, fmtInt, fmtMs, fmtNum, fmtPct, nodeCount, segmentFill, selfCostTotal, selfShare, totalCost, type Insight } from './explainModel';
import { ANALYZE_NEEDS, type RunGate } from './runGate';

interface Props {
  root: PlanNode;
  costs: Map<PlanNode, number | null>;
  warnings: Map<PlanNode, Insight[]>;
  hot: PlanNode | null;
  selected: PlanNode | null;
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

/**
 * The one-line overview above the plan: totals, the interactive self-cost distribution, and the
 * hottest node. A segment click selects that node; the bar carries the role of the former
 * "hot node" cards without taking another row.
 */
export function ExplainOverview({ root, costs, warnings, hot, selected, gate, onSelect, onAnalyze }: Props) {
  const total = totalCost(root, costs);
  const shareTotal = selfCostTotal(costs);
  const segments = useMemo(() => costSegments(root, costs), [root, costs]);
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

      {segments.length > 0 && (
        <div className="ex-costbar-wrap">
          <div className="ex-costbar-label">
            <span>Self cost distribution</span>
            <span>{fmtPct(segments[0]?.share ?? null)} top</span>
          </div>
          <div className="ex-costbar" role="group" aria-label="Self cost distribution">
            {segments.map((s, rank) =>
              s.node === null ? (
                <span
                  key={s.key}
                  className="ex-seg"
                  style={{
                    width: `${s.share * 100}%`,
                    background: segmentFill({ hot: false, rank, count: segments.length, other: true }),
                  }}
                  title={`${s.label} — ${fmtNum(s.cost)} (${fmtPct(s.share)})`}
                />
              ) : (
                <button
                  key={s.key}
                  type="button"
                  className="ex-seg"
                  aria-pressed={s.node === selected}
                  aria-label={`${s.label}: self cost ${fmtNum(s.cost)}, ${fmtPct(s.share)}`}
                  title={`${s.label} — ${fmtNum(s.cost)} (${fmtPct(s.share)}). Click to select.`}
                  style={{
                    width: `${s.share * 100}%`,
                    background: segmentFill({ hot: s.node === hot, rank, count: segments.length, other: false }),
                  }}
                  onClick={() => onSelect(s.node!)}
                />
              ),
            )}
          </div>
        </div>
      )}

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
