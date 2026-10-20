import { useMemo } from 'react';
import type { Value } from '../../bindings';
import { CHUNK, type ResultSet } from '../../store/queryRun';
import { cellView } from './format';
import { plainText } from './gridCopy';
import { JsonTree, parseJson } from './JsonTree';

function FieldValue({ value }: { value: Value }) {
  const json = useMemo(() => (value.t === 'Json' ? parseJson(value.v) : null), [value]);
  if (json && typeof json.value === 'object' && json.value !== null) return <JsonTree value={json.value} />;
  const view = cellView(value);
  if (view.kind === 'null') return <span className="rv-null">NULL</span>;
  return <pre className="rv-text">{value.t === 'Json' ? value.v : view.text}</pre>;
}

/**
 * Side panel with one row as `column → value` (read-only): Prev / Next move the grid's current row,
 * each field has its own copy button, JSON objects and arrays are collapsible trees.
 */
export function RecordView({
  result,
  row,
  rowNumber,
  onMove,
  onClose,
  onCopy,
}: {
  result: ResultSet;
  /** 0-based row shown. */
  row: number;
  /** Its number in the grid (the Data tab counts from its page offset). */
  rowNumber: number;
  onMove(delta: -1 | 1): void;
  onClose(): void;
  onCopy(text: string, what: string): void;
}) {
  const cells = result.chunks[Math.floor(row / CHUNK)]?.[row % CHUNK];
  return (
    <aside className="gt-record" aria-label="Record" data-testid="record-view">
      <header className="gt-record-head">
        <strong>Row {rowNumber.toLocaleString('en-US')}</strong>
        <span className="gt-spacer" />
        <button type="button" className="gt-btn" aria-label="Previous row" title="Previous row (Ctrl+Up)" disabled={row <= 0} onClick={() => onMove(-1)}>
          ‹ Prev
        </button>
        <button
          type="button"
          className="gt-btn"
          aria-label="Next row"
          title="Next row (Ctrl+Down)"
          disabled={row >= result.rowCount - 1}
          onClick={() => onMove(1)}
        >
          Next ›
        </button>
        <button type="button" className="gt-btn" aria-label="Close record view" title="Close (Esc)" onClick={onClose}>
          ✕
        </button>
      </header>
      <div className="gt-record-body">
        {!cells ? (
          <p className="gt-record-empty">This row is not loaded.</p>
        ) : (
          result.columns.map((c, i) => {
            const value = cells[i] ?? { t: 'Null' as const };
            return (
              <section className="gt-field" key={i} aria-label={c.name}>
                <div className="gt-field-head">
                  <span className="gt-field-name">{c.name}</span>
                  <span className="gt-field-type">{c.db_type}</span>
                  <span className="gt-spacer" />
                  <button
                    type="button"
                    className="gt-btn"
                    aria-label={`Copy ${c.name}`}
                    disabled={value.t === 'Null'}
                    onClick={() => onCopy(plainText(value), c.name)}
                  >
                    Copy
                  </button>
                </div>
                <div className="gt-field-value">
                  <FieldValue value={value} />
                </div>
              </section>
            );
          })
        )}
      </div>
    </aside>
  );
}
