import { useMemo, type ReactNode } from 'react';
import {
  accessTone,
  classicSummary,
  explainRow,
  type ClassicTable,
} from './classicPlan';
import { fmtInt, fmtMs, slowHints } from './explainModel';

/** Columns whose values are numbers (right-aligned). */
const NUMERIC = new Set(['id', 'key_len', 'rows', 'filtered']);

/** A cell with its known-slow parts tinted (`ALL`, filesort, temporary). */
export function SlowValue({ column, value }: { column: string; value: string }) {
  const hints = slowHints(column, value);
  if (hints.length === 0) return <>{value}</>;
  const parts: ReactNode[] = [];
  let from = 0;
  for (const h of hints) {
    if (h.start > from) parts.push(value.slice(from, h.start));
    parts.push(
      <span key={h.start} className="ex-slow" data-testid="explain-slow" title={h.reason}>
        {value.slice(h.start, h.end)}
      </span>,
    );
    from = h.end;
  }
  if (from < value.length) parts.push(value.slice(from));
  return <>{parts}</>;
}

function Cell({ column, value }: { column: string; value: string | null }) {
  if (value === null) return <span className="ex-null">(Null)</span>;
  if (column.toLowerCase() === 'type') {
    const tone = accessTone(value);
    if (tone === 'good') {
      return (
        <span className="ex-good" title="One row per lookup or better">
          {value}
        </span>
      );
    }
    if (value === 'index') {
      return (
        <span className="ex-slow" data-testid="explain-slow" title="Full index scan">
          {value}
        </span>
      );
    }
  }
  return <SlowValue column={column} value={value} />;
}

/**
 * The line under a plan about its selected row or node: the name, its warnings (warning colour),
 * then the plain-words reading. One line; the whole text is in the tooltip.
 */
export function ExplainFoot({ label, warnings, parts }: { label: string; warnings: string[]; parts: string[] }) {
  const all = [...warnings.map((w) => `⚠ ${w}`), ...parts];
  return (
    <div className="ex-foot" data-testid="explain-foot" title={[label, ...all].join('\n')}>
      <span className="ex-foot-label">{label}</span>
      <span className="ex-foot-text">
        {warnings.map((w) => (
          <span key={w} className="warn">
            ⚠ {w}
          </span>
        ))}
        {parts.map((p, i) => (
          <span key={i}>{p}</span>
        ))}
      </span>
    </div>
  );
}

/**
 * The classic plan in one line for the Explain bar: tables, rows read on the first one, then the
 * warnings (full scans, filesort, temporary) in the warning colour.
 */
export function ClassicSummary({ table }: { table: ClassicTable }) {
  const summary = useMemo(() => classicSummary(table), [table]);
  const warnings = [
    ...(summary.fullScans > 0 ? [`${summary.fullScans} full table scan${summary.fullScans === 1 ? '' : 's'}`] : []),
    ...summary.slowExtra,
  ];
  return (
    <span className="ex-summary" data-testid="explain-summary">
      <span>
        {summary.tables} table{summary.tables === 1 ? '' : 's'}
      </span>
      {summary.firstRows !== null && <span>≈ {fmtInt(summary.firstRows)} rows read on the first table</span>}
      {warnings.map((w) => (
        <span key={w} className="warn">
          {w}
        </span>
      ))}
    </span>
  );
}

interface Props {
  table: ClassicTable;
  filter: string;
  selected: number;
  /** Analyze ran: the root's time (the grid itself shows estimates). */
  analyzedMs: number | null;
  onSelect(index: number): void;
  onOpenTree(): void;
}

/**
 * The classic EXPLAIN grid of MySQL / MariaDB (Navicat-style): the server's columns, (Null)
 * like the result grid, slow values tinted, and the selected row read in plain words (its
 * summary line is in the Explain bar: `ClassicSummary`).
 */
export function ExplainClassic({ table, filter, selected, analyzedMs, onSelect, onOpenTree }: Props) {
  const q = filter.trim().toLowerCase();
  const rows = table.rows
    .map((row, index) => ({ row, index }))
    .filter(({ row }) => q.length === 0 || row.some((v) => v !== null && v.toLowerCase().includes(q)));
  const current = table.rows.length === 0 ? null : Math.min(selected, table.rows.length - 1);
  const reading = current === null ? null : explainRow(table, current);

  return (
    <>
      <div className="ex-main">
        <div className="ex-pane ex-classic">
          {analyzedMs !== null && (
            <div className="ex-note" role="note">
              <span>Analyzed in {fmtMs(analyzedMs)}. The grid shows the optimizer’s estimates.</span>
              <button type="button" className="ex-link" onClick={onOpenTree}>
                Open Tree for actual rows and time
              </button>
            </div>
          )}
          <div className="ex-scroll">
            {rows.length === 0 ? (
              <div className="ex-empty">No rows match “{filter.trim()}”.</div>
            ) : (
              <table className="ex-grid" data-testid="explain-table">
                <thead>
                  <tr>
                    <th scope="col" className="ex-gutter">
                      #
                    </th>
                    {table.columns.map((c, i) => (
                      <th scope="col" key={i} className={NUMERIC.has(c.toLowerCase()) ? 'r' : undefined}>
                        {c}
                      </th>
                    ))}
                  </tr>
                </thead>
                <tbody>
                  {rows.map(({ row, index }) => (
                    <tr
                      key={index}
                      data-testid="explain-classic-row"
                      className={index === current ? 'sel' : undefined}
                      aria-selected={index === current}
                      tabIndex={0}
                      onClick={() => onSelect(index)}
                      onKeyDown={(e) => {
                        if (e.key === 'Enter' || e.key === ' ') {
                          e.preventDefault();
                          onSelect(index);
                        }
                      }}
                    >
                      <td className="ex-gutter">{index + 1}</td>
                      {row.map((v, j) => {
                        const column = table.columns[j] ?? '';
                        return (
                          <td key={j} className={NUMERIC.has(column.toLowerCase()) ? 'r' : undefined} title={v ?? 'NULL'}>
                            <Cell column={column} value={v} />
                          </td>
                        );
                      })}
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </div>
        </div>

      </div>

      {reading !== null && current !== null && (
        <ExplainFoot
          label={`Row ${current + 1}${reading.table !== null ? ` · ${reading.table}` : ''}`}
          warnings={reading.extra.filter((x) => x.slow && x.note !== null).map((x) => x.note!)}
          parts={[reading.access, reading.index]}
        />
      )}
    </>
  );
}
