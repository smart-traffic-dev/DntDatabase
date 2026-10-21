import { useMemo, type ReactNode } from 'react';
import {
  ACCESS_LADDER,
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

/** Where `type` sits from best to worst. */
function Ladder({ current }: { current?: string | null }) {
  return (
    <span className="ex-ladder" role="img" aria-label={`Access types from best to worst${current ? `; this row: ${current}` : ''}`}>
      {ACCESS_LADDER.map((t) => (
        <span key={t} className={t === current ? 'cur' : undefined}>
          {t}
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
 * like the result grid, slow values tinted, and the selected row read in plain words.
 */
export function ExplainClassic({ table, filter, selected, analyzedMs, onSelect, onOpenTree }: Props) {
  const summary = useMemo(() => classicSummary(table), [table]);
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
          <div className="ex-chips" data-testid="explain-summary">
            <span className="ex-chip">
              {summary.tables} table{summary.tables === 1 ? '' : 's'}
            </span>
            {summary.firstRows !== null && (
              <span className="ex-chip">≈ {fmtInt(summary.firstRows)} rows read on the first table</span>
            )}
            {summary.fullScans > 0 && (
              <span className="ex-chip warn">
                ⚠ {summary.fullScans} full table scan{summary.fullScans === 1 ? '' : 's'}
              </span>
            )}
            {summary.slowExtra.map((s) => (
              <span key={s} className="ex-chip warn">
                {s}
              </span>
            ))}
          </div>
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
            <div className="ex-legend">
              <span>type, best → worst</span>
              <Ladder />
              <span className="ex-legend-hint">Click a row to read it in plain words.</span>
            </div>
          </div>
        </div>

        {reading !== null && current !== null && (
          <aside className="ex-details" data-testid="explain-details" aria-label="Selected row">
            <div className="ex-details-head">
              <div className="ex-details-title">
                <h3>
                  Row {current + 1}
                  {reading.table !== null && ` · ${reading.table}`}
                  {reading.type === 'ALL' && <span className="ex-hotpill">FULL SCAN</span>}
                </h3>
              </div>
            </div>
            <div className="ex-details-scroll">
              <section className="ex-section">
                <h4>Access</h4>
                <Ladder current={reading.type} />
                <p className="ex-say">{reading.access}</p>
              </section>
              <section className="ex-section">
                <h4>Index</h4>
                <div className="ex-kv">
                  <span>Possible keys</span>
                  <b>{reading.possibleKeys ?? '—'}</b>
                </div>
                <div className="ex-kv">
                  <span>Key used</span>
                  <b>{reading.key ?? 'none'}</b>
                </div>
                <p className="ex-say">{reading.index}</p>
              </section>
              <section className="ex-section">
                <h4>Rows</h4>
                <div className="ex-kv">
                  <span>Examined</span>
                  <b>{reading.examined === null ? '—' : `≈ ${fmtInt(reading.examined)}`}</b>
                </div>
                <div className="ex-kv">
                  <span>Pass the condition{reading.filtered === null ? '' : ` (filtered ${reading.filtered}%)`}</span>
                  <b>{reading.passing === null ? '—' : `≈ ${fmtInt(reading.passing)}`}</b>
                </div>
              </section>
              {reading.extra.length > 0 && (
                <section className="ex-section">
                  <h4>Extra</h4>
                  {reading.extra.map((x, i) => (
                    <div className="ex-extra" key={i}>
                      <code className={x.slow ? 'warn' : undefined}>{x.text}</code>
                      {x.note !== null && <span>{x.note}</span>}
                    </div>
                  ))}
                </section>
              )}
            </div>
          </aside>
        )}
      </div>

      {reading !== null && current !== null && (
        <div className="ex-foot" data-testid="explain-foot">
          <span className="ex-foot-label">
            Row {current + 1}
            {reading.table !== null && ` · ${reading.table}`}
          </span>
          <span>{reading.access}</span>
        </div>
      )}
    </>
  );
}
