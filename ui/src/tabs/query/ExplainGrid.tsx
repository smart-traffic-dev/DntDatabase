import { useEffect, useMemo, useState } from 'react';
import type { PlanNode } from '../../bindings';
import { fmtInt, fmtMs, fmtNum, flattenPlan, iconOf, matchesFilter, rowsRatio, type FlatNode, type Insight } from './explainModel';
import { Highlight } from './ExplainHighlight';

type SortKey = 'plan' | 'cost' | 'self' | 'est' | 'actual' | 'ratio' | 'time' | 'loops';
type Sort = { key: SortKey; dir: 'asc' | 'desc' };

interface Props {
  root: PlanNode;
  costs: Map<PlanNode, number | null>;
  warnings: Map<PlanNode, Insight[]>;
  hot: PlanNode | null;
  selected: PlanNode | null;
  filter: string;
  onSelect(node: PlanNode): void;
}

const guidesOf = (f: FlatNode): string => {
  if (f.depth === 0) return '';
  const above = f.guides.map((open) => (open ? '│  ' : '   ')).join('');
  return `${above}${f.last ? '└─ ' : '├─ '}`;
};

const ratioText = (node: PlanNode): string => {
  const ratio = rowsRatio(node);
  if (ratio === null) return '—';
  return ratio >= 10 ? `${fmtInt(ratio)}×` : `${Math.round(ratio * 100) / 100}×`;
};

const valueOf = (f: FlatNode & { order: number }, key: SortKey, costs: Map<PlanNode, number | null>): number | null => {
  switch (key) {
    case 'plan':
      return f.order;
    case 'cost':
      return f.node.est_cost;
    case 'self':
      return costs.get(f.node) ?? null;
    case 'est':
      return f.node.est_rows;
    case 'actual':
      return f.node.actual_rows;
    case 'ratio':
      return rowsRatio(f.node);
    case 'time':
      return f.node.actual_ms;
    case 'loops':
      return f.node.loops;
  }
};

function SortHeader({
  k,
  label,
  className,
  sort,
  onSort,
}: {
  k: SortKey;
  label: string;
  className?: string;
  sort: Sort;
  onSort(key: SortKey): void;
}) {
  const active = sort.key === k;
  return (
    <th
      scope="col"
      className={className}
      aria-sort={active ? (sort.dir === 'asc' ? 'ascending' : 'descending') : 'none'}
    >
      <button type="button" className="ex-sort" onClick={() => onSort(k)}>
        {label}
        <span className="ex-sort-arrow" aria-hidden="true">
          {active ? (sort.dir === 'asc' ? '↑' : '↓') : '↕'}
        </span>
      </button>
    </th>
  );
}

/**
 * The node grid (PostgreSQL, or a MySQL / MariaDB plan without its classic table): one row per
 * node, tree guides in the first column, sortable numeric columns.
 */
export function ExplainGrid({ root, costs, warnings, hot, selected, filter, onSelect }: Props) {
  const [sort, setSort] = useState<Sort>({ key: 'plan', dir: 'asc' });
  useEffect(() => {
    setSort({ key: 'plan', dir: 'asc' });
  }, [root]);

  const onSort = (key: SortKey) => {
    setSort((prev) =>
      prev.key === key
        ? key === 'plan'
          ? prev
          : { key, dir: prev.dir === 'asc' ? 'desc' : 'asc' }
        : { key, dir: key === 'plan' ? 'asc' : 'desc' },
    );
  };

  const flat = useMemo(() => flattenPlan(root), [root]);
  const ordered = useMemo(() => flat.map((f, order) => ({ ...f, order })), [flat]);
  const rows = useMemo(() => {
    const q = filter.trim();
    const filtered = ordered.filter((f) => q.length === 0 || matchesFilter(f.node, q));
    if (sort.key === 'plan') return filtered;
    const sorted = [...filtered];
    sorted.sort((a, b) => {
      const av = valueOf(a, sort.key, costs);
      const bv = valueOf(b, sort.key, costs);
      if (av === null && bv === null) return a.order - b.order;
      if (av === null) return 1;
      if (bv === null) return -1;
      return (av - bv) * (sort.dir === 'asc' ? 1 : -1) || a.order - b.order;
    });
    return sorted;
  }, [ordered, filter, sort, costs]);

  return (
    <div className="ex-scroll">
      {rows.length === 0 ? (
        <div className="ex-empty">No nodes match “{filter.trim()}”.</div>
      ) : (
        <table className="ex-grid" data-testid="explain-grid">
          <thead>
            <tr>
              <SortHeader k="plan" label="#" className="ex-gutter" sort={sort} onSort={onSort} />
              <th scope="col">Node</th>
              <th scope="col" className="ex-detail-col">
                Detail
              </th>
              <SortHeader k="cost" label="Cost" className="r" sort={sort} onSort={onSort} />
              <SortHeader k="self" label="Self" className="r" sort={sort} onSort={onSort} />
              <SortHeader k="est" label="Est. rows" className="r" sort={sort} onSort={onSort} />
              <SortHeader
                k="actual"
                label="Actual"
                className="r ex-col-actual"
                sort={sort}
                onSort={onSort}
              />
              <SortHeader
                k="ratio"
                label="×"
                className="r ex-col-actual"
                sort={sort}
                onSort={onSort}
              />
              <SortHeader
                k="time"
                label="Time"
                className="r ex-col-time"
                sort={sort}
                onSort={onSort}
              />
              <SortHeader
                k="loops"
                label="Loops"
                className="r ex-col-loops"
                sort={sort}
                onSort={onSort}
              />
              <th scope="col" className="r">
                Warn
              </th>
            </tr>
          </thead>
          <tbody>
            {rows.map((f) => {
              const node = f.node;
              const own = costs.get(node) ?? null;
              const found = warnings.get(node) ?? [];
              const icon = iconOf(node.label);
              return (
                <tr
                  key={f.order}
                  data-testid="explain-row"
                  className={`${node === hot ? 'hot' : ''}${node === selected ? ' sel' : ''}`.trim()}
                  tabIndex={0}
                  onClick={() => onSelect(node)}
                  onKeyDown={(e) => {
                    if (e.key === 'Enter' || e.key === ' ') {
                      e.preventDefault();
                      onSelect(node);
                    }
                  }}
                >
                  <td className="ex-gutter">{f.order + 1}</td>
                  <td>
                    <span className="ex-nodecell">
                      <span className="ex-guides" aria-hidden="true">
                        {guidesOf(f)}
                      </span>
                      <span className={`ex-ico ${icon.kind}`} aria-hidden="true">
                        {icon.glyph}
                      </span>
                      <span className="ex-label" title={node.label}>
                        <Highlight text={node.label} filter={filter} />
                      </span>
                      {node === hot && <span className="ex-hotpill">HOT</span>}
                    </span>
                  </td>
                  <td className="ex-detail-cell ex-detail-col" title={node.detail ?? undefined}>
                    {node.detail === null ? '—' : <Highlight text={node.detail} filter={filter} />}
                  </td>
                  <td className="r">{fmtNum(node.est_cost)}</td>
                  <td className="r">{fmtNum(own)}</td>
                  <td className="r">{fmtInt(node.est_rows)}</td>
                  <td className={`r ex-col-actual${found.some((w) => w.kind === 'skew') ? ' skew' : ''}`}>
                    {fmtInt(node.actual_rows)}
                  </td>
                  <td className="r ex-col-actual">{ratioText(node)}</td>
                  <td className="r ex-col-time">{fmtMs(node.actual_ms)}</td>
                  <td className="r ex-col-loops">{fmtInt(node.loops)}</td>
                  <td className="r">
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
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      )}
    </div>
  );
}
