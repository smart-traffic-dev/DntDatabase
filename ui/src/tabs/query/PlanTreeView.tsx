import { useMemo } from 'react';
import type { PlanNode } from '../../bindings';
import { barFill, fmtInt, fmtMs, fmtNum, fmtPct, filterTree, flattenPlan, iconOf, maxSelfCost, selfShare, type FlatNode, type Insight } from './explainModel';
import { Highlight } from './ExplainHighlight';

interface Props {
  root: PlanNode;
  costs: Map<PlanNode, number | null>;
  warnings: Map<PlanNode, Insight[]>;
  hot: PlanNode | null;
  selected: PlanNode | null;
  collapsed: Set<PlanNode>;
  filter: string;
  total: number | null;
  /** The engine reports time and rows per loop (the headers say so). */
  perLoop: boolean;
  onToggle(node: PlanNode): void;
  onSelect(node: PlanNode): void;
}

/** The indent and connectors in front of a node (`├─`, `└─`, with `│` under open branches). */
function guidesOf(f: FlatNode): string {
  if (f.depth === 0) return '';
  const above = f.guides.map((open) => (open ? '│  ' : '   ')).join('');
  return `${above}${f.last ? '└─ ' : '├─ '}`;
}

/** The tree view: a column-aligned plan tree with self-cost bars, warnings and a selected row. */
export function PlanTreeView({
  root,
  costs,
  warnings,
  hot,
  selected,
  collapsed,
  filter,
  total,
  perLoop,
  onToggle,
  onSelect,
}: Props) {
  const flat = useMemo(() => flattenPlan(root), [root]);
  const shown = useMemo(() => {
    const filtered = filterTree(root, filter);
    if (filtered !== null) return flat.filter((f) => filtered.has(f.node));
    const open = new Set<PlanNode>();
    const visit = (node: PlanNode) => {
      open.add(node);
      if (!collapsed.has(node)) node.children.forEach(visit);
    };
    visit(root);
    return flat.filter((f) => open.has(f.node));
  }, [flat, root, filter, collapsed]);
  const maxSelf = useMemo(() => maxSelfCost(costs), [costs]);

  return (
    <div className="ex-scroll">
      <div className="ex-cols ex-head" aria-hidden="true">
        <span>Node</span>
        <span className="r">Cost</span>
        <span className="r">Self cost</span>
        <span className="r">Est. rows</span>
        <span className="r ex-col-actual" title={perLoop ? 'Actual rows per loop' : 'Actual rows'}>
          Actual
        </span>
        <span className="r ex-col-time" title={perLoop ? 'Time per loop' : 'Time in total'}>
          Time
        </span>
        <span className="r ex-col-loops">Loops</span>
        <span className="r">Warn</span>
      </div>
      {shown.length === 0 ? (
        <div className="ex-empty">No nodes match “{filter.trim()}”.</div>
      ) : (
        <ul className="ex-list" role="tree" aria-label="Plan tree">
          {shown.map((f, rowIndex) => {
            const node = f.node;
            const own = costs.get(node) ?? null;
            const share = selfShare(node, costs, total);
            const isHot = node === hot;
            const isOpen = filter.trim().length > 0 ? true : !collapsed.has(node);
            const hasChildren = node.children.length > 0;
            const found = warnings.get(node) ?? [];
            const icon = iconOf(node.label);
            const heatT = own !== null && maxSelf > 0 ? Math.min(1, own / maxSelf) : 0;
            return (
              <li
                key={rowIndex}
                role="treeitem"
                aria-level={f.depth + 1}
                aria-selected={node === selected}
                aria-expanded={hasChildren ? isOpen : undefined}
                tabIndex={0}
                data-testid="explain-node"
                className={`ex-cols ex-row${isHot ? ' hot' : ''}${node === selected ? ' sel' : ''}`}
                onClick={() => onSelect(node)}
                onKeyDown={(e) => {
                  if (e.key === 'Enter' || e.key === ' ') {
                    e.preventDefault();
                    onSelect(node);
                  }
                }}
              >
                <span className="ex-nodecell">
                  <span className="ex-guides" aria-hidden="true">
                    {guidesOf(f)}
                  </span>
                  {hasChildren ? (
                    <button
                      type="button"
                      className="ex-caret"
                      aria-expanded={isOpen}
                      aria-label={`${isOpen ? 'Collapse' : 'Expand'} ${node.label}`}
                      disabled={filter.trim().length > 0}
                      title={filter.trim().length > 0 ? 'Clear the filter to collapse branches' : undefined}
                      onClick={(e) => {
                        e.stopPropagation();
                        onToggle(node);
                      }}
                    >
                      {isOpen ? '▾' : '▸'}
                    </button>
                  ) : (
                    <span className="ex-caret empty" aria-hidden="true" />
                  )}
                  <span className={`ex-ico ${icon.kind}`} aria-hidden="true">
                    {icon.glyph}
                  </span>
                  <span className="ex-label" title={node.label}>
                    <Highlight text={node.label} filter={filter} />
                  </span>
                  {node.detail !== null && (
                    <span className="ex-sub" title={node.detail}>
                      <Highlight text={node.detail} filter={filter} />
                    </span>
                  )}
                  {isHot && <span className="ex-hotpill">HOT</span>}
                </span>
                <span className="ex-cell r strong">{fmtNum(node.est_cost)}</span>
                <span
                  className="ex-cell r ex-self"
                  title={`${fmtNum(own)}${share === null ? '' : ` · ${fmtPct(share)} of self cost`}`}
                >
                  <span
                    className="ex-selfbar"
                    style={{ width: `${own === null ? 0 : heatT * 100}%`, background: barFill(isHot) }}
                  />
                  <span className="ex-selfval">{fmtNum(own)}</span>
                </span>
                <span className="ex-cell r">{fmtInt(node.est_rows)}</span>
                <span className={`ex-cell r ex-col-actual${found.some((w) => w.kind === 'skew') ? ' skew' : ''}`}>
                  {fmtInt(node.actual_rows)}
                </span>
                <span className="ex-cell r ex-col-time">{fmtMs(node.actual_ms)}</span>
                <span className="ex-cell r ex-col-loops">{fmtInt(node.loops)}</span>
                <span className="ex-warncell">
                  {found.length > 0 && (
                    <span
                      className="ex-warnpill"
                      data-testid="explain-warning"
                      role="img"
                      aria-label={`${found.length} warning${found.length === 1 ? '' : 's'}`}
                      title={found.map((w) => `${w.title}: ${w.body}`).join('\n')}
                    >
                      ⚠ {found.length}
                    </span>
                  )}
                </span>
              </li>
            );
          })}
        </ul>
      )}
    </div>
  );
}
