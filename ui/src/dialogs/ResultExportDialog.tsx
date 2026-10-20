import { Channel } from '@tauri-apps/api/core';
import { save } from '@tauri-apps/plugin-dialog';
import { useRef, useState, type FormEvent } from 'react';
import { api, isCancelled, toAppError } from '../api';
import type { ExportFormat, ExportOptions, ExportProgress, ExportSummary } from '../bindings';
import { Modal } from './Modal';
import { FORMATS, defaultOptions, formatInfo, type ExportRequest } from './resultExport';

/** CSV delimiters offered (value sent as is). */
export const DELIMITERS: { value: string; label: string }[] = [
  { value: ',', label: 'Comma (,)' },
  { value: ';', label: 'Semicolon (;)' },
  { value: '\t', label: 'Tab' },
  { value: '|', label: 'Pipe (|)' },
];

type Scope = 'loaded' | 'all';

type Phase =
  | { kind: 'form' }
  | { kind: 'running'; rows: number; key: string | null }
  | { kind: 'done'; summary: ExportSummary }
  | { kind: 'cancelled' };

let exports = 0;
const fmt = (n: number) => n.toLocaleString('en-US');

interface Props {
  request: ExportRequest;
  onClose(): void;
}

/**
 * Export a result to a file: CSV / JSON / Excel / SQL INSERT / Markdown, the loaded rows or all
 * rows. "All rows" is read again by the backend and streamed straight to the file, with progress
 * and Cancel; the file is written only by the backend.
 */
export function ResultExportDialog({ request, onClose }: Props) {
  const [opts, setOpts] = useState<ExportOptions>(() => defaultOptions(request.tableName));
  const [scope, setScope] = useState<Scope>('loaded');
  const [phase, setPhase] = useState<Phase>({ kind: 'form' });
  const [error, setError] = useState<string | null>(null);
  const run = useRef(0);

  const set = (change: Partial<ExportOptions>) => setOpts((o) => ({ ...o, ...change }));
  const info = formatInfo(opts.format);
  const busy = phase.kind === 'running';
  const tableMissing = opts.format === 'sql' && opts.table_name.trim() === '';
  const canExport = !busy && !tableMissing && (scope === 'loaded' || request.all !== null);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    if (!canExport) return;
    const my = ++run.current;
    setError(null);
    const path = await save({
      defaultPath: `${request.baseName}.${info.ext}`,
      filters: [{ name: info.filter, extensions: [info.ext] }],
    });
    if (my !== run.current) return;
    if (!path) return;
    const all = scope === 'all' ? request.all : null;
    const key = all ? `export:${Date.now()}:${++exports}` : null;
    setPhase({ kind: 'running', rows: 0, key });
    try {
      let summary: ExportSummary;
      if (!all) {
        summary = await api.exportRows(request.engine, request.columns, request.rows(), opts, path);
      } else {
        const channel = new Channel<ExportProgress>();
        channel.onmessage = (p) => {
          if (my === run.current) setPhase((ph) => (ph.kind === 'running' ? { ...ph, rows: p.rows } : ph));
        };
        summary =
          all.kind === 'table'
            ? await api.exportTable(all.connectionId, all.obj, all.sort, all.filter, opts, path, key!, channel)
            : await api.exportQuery(all.planId, all.index, opts, path, key!, channel);
      }
      if (my === run.current) setPhase({ kind: 'done', summary });
    } catch (err) {
      if (my !== run.current) return;
      const appError = toAppError(err);
      if (isCancelled(appError)) {
        setPhase({ kind: 'cancelled' });
      } else {
        setError(appError.message);
        setPhase({ kind: 'form' });
      }
    }
  };

  const cancel = () => {
    if (phase.kind !== 'running' || !phase.key || !request.all) return;
    // The backend stops the statement and removes the half-written file.
    api.tableDataCancel(request.all.connectionId, phase.key).catch(() => undefined);
  };

  const close = () => {
    cancel();
    run.current++;
    onClose();
  };

  const reveal = async (path: string) => {
    try {
      await api.revealExport(path);
    } catch (e) {
      setError(toAppError(e).message);
    }
  };

  if (phase.kind === 'done' || phase.kind === 'cancelled') {
    const s = phase.kind === 'done' ? phase.summary : null;
    return (
      <Modal title={request.title} onClose={close} testId="result-export-dialog">
        <div className="modal-body stack" role="status">
          {s ? (
            <>
              <p>
                Exported {fmt(s.rows)} {s.rows === 1 ? 'row' : 'rows'} to <code>{s.path}</code>.
              </p>
              {s.cut_cells > 0 ? (
                <p className="hint">{fmt(s.cut_cells)} long values were cut to Excel's 32,767 characters.</p>
              ) : null}
            </>
          ) : (
            <p>The export was cancelled; no file was written.</p>
          )}
          {error && (
            <p role="alert" className="field-error">
              {error}
            </p>
          )}
        </div>
        <footer className="modal-foot">
          <span className="spacer" />
          {s ? (
            <button type="button" className="btn" onClick={() => void reveal(s.path)}>
              Open folder
            </button>
          ) : null}
          <button type="button" className="btn primary" onClick={close}>
            Close
          </button>
        </footer>
      </Modal>
    );
  }

  return (
    <Modal title={request.title} onClose={close} testId="result-export-dialog" initialFocus="select">
      <form onSubmit={(e) => void submit(e)}>
        <fieldset className="modal-body stack export-form" disabled={busy}>
          <div className="field">
            <label htmlFor="rx-format">Format</label>
            <select id="rx-format" value={opts.format} onChange={(e) => set({ format: e.target.value as ExportFormat })}>
              {FORMATS.map((f) => (
                <option key={f.format} value={f.format}>
                  {f.label}
                </option>
              ))}
            </select>
          </div>
          {opts.format === 'csv' ? (
            <>
              <div className="field">
                <label htmlFor="rx-delim">Delimiter</label>
                <select id="rx-delim" value={opts.delimiter} onChange={(e) => set({ delimiter: e.target.value })}>
                  {DELIMITERS.map((d) => (
                    <option key={d.label} value={d.value}>
                      {d.label}
                    </option>
                  ))}
                </select>
              </div>
              <label className="check">
                <input type="checkbox" checked={opts.header} onChange={(e) => set({ header: e.target.checked })} />
                Header row
              </label>
              <label className="check">
                <input type="checkbox" checked={opts.quote_all} onChange={(e) => set({ quote_all: e.target.checked })} />
                Quote all fields
              </label>
              <label className="check">
                <input type="checkbox" checked={opts.bom} onChange={(e) => set({ bom: e.target.checked })} />
                UTF-8 BOM (for Excel)
              </label>
            </>
          ) : null}
          {opts.format === 'sql' ? (
            <div className="field">
              <label htmlFor="rx-table">Table name</label>
              <input
                id="rx-table"
                value={opts.table_name}
                aria-invalid={tableMissing}
                onChange={(e) => set({ table_name: e.target.value })}
              />
              <p className="hint">Quoted as one identifier for {request.engine === 'postgres' ? 'PostgreSQL' : 'MySQL'}.</p>
            </div>
          ) : null}
          <div className="field" role="radiogroup" aria-label="Rows">
            <span className="field-label">Rows</span>
            <label className="check">
              <input type="radio" name="rx-scope" checked={scope === 'loaded'} onChange={() => setScope('loaded')} />
              Loaded rows ({fmt(request.loadedCount)})
            </label>
            <label className="check">
              <input
                type="radio"
                name="rx-scope"
                checked={scope === 'all'}
                disabled={request.all === null}
                onChange={() => setScope('all')}
              />
              All rows (read again)
            </label>
            {request.all === null && request.allUnavailable ? <p className="hint">{request.allUnavailable}</p> : null}
            {scope === 'loaded' && request.loadedNote ? <p className="hint">{request.loadedNote}</p> : null}
          </div>
          {error && (
            <p role="alert" className="field-error">
              {error}
            </p>
          )}
        </fieldset>
        <footer className="modal-foot">
          <span className="hint spacer" aria-live="polite">
            {busy ? `Exporting… ${fmt(phase.rows)} rows written` : null}
          </span>
          {busy ? (
            <button type="button" className="btn" onClick={cancel} disabled={!phase.key}>
              Cancel export
            </button>
          ) : (
            <button type="button" className="btn" onClick={close}>
              Cancel
            </button>
          )}
          <button type="submit" className="btn primary" disabled={!canExport}>
            Export
          </button>
        </footer>
      </form>
    </Modal>
  );
}
