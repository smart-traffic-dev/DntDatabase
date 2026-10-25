import { useState } from 'react';
import { api, toAppError } from '../../../api';
import type { Cell, SourceFormat } from '../../../bindings';
import type { StepProps } from './stepProps';

const FORMATS: { value: SourceFormat; label: string }[] = [
  { value: 'csv', label: 'CSV / TSV' },
  { value: 'json', label: 'JSON (array of objects)' },
  { value: 'jsonl', label: 'JSON Lines' },
  { value: 'xlsx', label: 'Excel workbook (XLSX)' },
];

const DELIMITERS: { value: string; label: string }[] = [
  { value: '', label: 'Detect' },
  { value: ',', label: 'Comma ( , )' },
  { value: ';', label: 'Semicolon ( ; )' },
  { value: '\t', label: 'Tab' },
  { value: '|', label: 'Pipe ( | )' },
];

const delimiterName = (d: string) => DELIMITERS.find((x) => x.value === d)?.label ?? d;

/** One source cell as the preview shows it (text, as read; NULL marked). */
export function CellText({ cell }: { cell: Cell }) {
  if (cell.t === 'Null') return <span className="import-null">NULL</span>;
  if (cell.t === 'Bool') return <>{cell.v ? 'true' : 'false'}</>;
  return <>{cell.v}</>;
}

interface Props extends StepProps {
  /** The inspection failed (the file cannot be read with these options). */
  inspectError: string | null;
}

/** Step 1: the file, its format options and a preview of its first 100 records. */
export function SourceStep({ state, dispatch, inspectError }: Props) {
  const [pickError, setPickError] = useState<string | null>(null);
  const [picking, setPicking] = useState(false);
  const { source, inspection } = state;

  const choose = async () => {
    setPickError(null);
    setPicking(true);
    try {
      // The backend opens the dialog and admits only the picked file for the import commands.
      const path = await api.importPickFile();
      if (path) dispatch({ type: 'file', path });
    } catch (e) {
      setPickError(toAppError(e).message);
    } finally {
      setPicking(false);
    }
  };

  const csv = source.format === 'csv';
  const xlsx = source.format === 'xlsx';

  return (
    <div className="stack">
      <div className="field">
        <label htmlFor="iw-file">File</label>
        <div className="row">
          <input id="iw-file" readOnly value={state.path ?? ''} placeholder="No file selected" />
          <button type="button" className="btn" disabled={picking} onClick={() => void choose()}>
            Choose file…
          </button>
        </div>
      </div>

      {state.path && (
        <div className="import-wizard-options">
          <div className="field">
            <label htmlFor="iw-format">Format</label>
            <select id="iw-format" value={source.format} onChange={(e) => dispatch({ type: 'source', patch: { format: e.target.value as SourceFormat, sheet: null } })}>
              {FORMATS.map((f) => (
                <option key={f.value} value={f.value}>
                  {f.label}
                </option>
              ))}
            </select>
          </div>
          {csv && (
            <div className="field">
              <label htmlFor="iw-delimiter">Delimiter</label>
              <select id="iw-delimiter" value={source.delimiter ?? ''} onChange={(e) => dispatch({ type: 'source', patch: { delimiter: e.target.value || null } })}>
                {DELIMITERS.map((d) => (
                  <option key={d.label} value={d.value}>
                    {d.label}
                  </option>
                ))}
              </select>
            </div>
          )}
          {xlsx && (
            <div className="field">
              <label htmlFor="iw-sheet">Sheet</label>
              <select id="iw-sheet" value={source.sheet ?? ''} onChange={(e) => dispatch({ type: 'source', patch: { sheet: e.target.value || null } })}>
                <option value="">First sheet</option>
                {state.sheets.map((s) => (
                  <option key={s} value={s}>
                    {s}
                  </option>
                ))}
              </select>
            </div>
          )}
          {(csv || xlsx) && (
            <label className="check import-wizard-check">
              <input type="checkbox" checked={source.header} onChange={(e) => dispatch({ type: 'source', patch: { header: e.target.checked } })} />
              <span>First row holds the column names</span>
            </label>
          )}
        </div>
      )}

      {pickError && (
        <p role="alert" className="field-error">
          {pickError}
        </p>
      )}
      {inspectError && (
        <p role="alert" className="field-error">
          {inspectError}
        </p>
      )}
      {state.path && !inspection && !inspectError && (
        <p role="status" className="hint">
          Reading the file…
        </p>
      )}

      {inspection && (
        <>
          <p className="hint">
            {inspection.columns.length} columns · first {inspection.rows.length} records
            {csv && inspection.delimiter && !source.delimiter ? ` · detected delimiter: ${delimiterName(inspection.delimiter)}` : ''}
          </p>
          <div className="import-wizard-grid">
            <table className="import-table" data-testid="import-preview">
              <thead>
                <tr>
                  <th className="import-rownum">#</th>
                  {inspection.columns.map((c, i) => (
                    <th key={`${c}:${i}`}>{c}</th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {inspection.rows.map((r) => (
                  <tr key={r.number}>
                    <td className="import-rownum">{r.number}</td>
                    {r.cells.map((cell, i) => (
                      <td key={i}>
                        <CellText cell={cell} />
                      </td>
                    ))}
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
          {inspection.rows.length === 0 && <p className="hint">The file has no records.</p>}
        </>
      )}
    </div>
  );
}
