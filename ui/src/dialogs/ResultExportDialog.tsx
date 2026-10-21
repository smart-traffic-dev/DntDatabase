import { Channel } from '@tauri-apps/api/core';
import { useRef, useState, type FormEvent } from 'react';
import { api, isCancelled, toAppError } from '../api';
import type { ExportFormat, ExportOptions, ExportProgress, ExportSummary } from '../bindings';
import { useQueryRun } from '../store/queryRun';
import { asTxState } from '../store/queryTabs';
import { Modal } from './Modal';
import { FORMATS, defaultOptions, type ExportRequest } from './resultExport';

/** CSV delimiters offered (value sent as is). */
export const DELIMITERS: { value: string; label: string }[] = [
  { value: ',', label: 'Comma (,)' },
  { value: ';', label: 'Semicolon (;)' },
  { value: '\t', label: 'Tab' },
  { value: '|', label: 'Pipe (|)' },
];

type Scope = 'loaded' | 'all';

/** How a running export is stopped: a table export by its request key, a stored query result by its operation id. */
type Stop = { kind: 'table'; connectionId: string; key: string } | { kind: 'stored'; operation: string } | null;

type Phase =
  | { kind: 'form' }
  | { kind: 'running'; rows: number; stop: Stop }
  | { kind: 'done'; summary: ExportSummary }
  | { kind: 'cancelled' };

let exports = 0;
const fmt = (n: number) => n.toLocaleString('en-US');
const plural = (n: number, one: string, many: string) => `${fmt(n)} ${n === 1 ? one : many}`;

interface Props {
  request: ExportRequest;
  onClose(): void;
}

/**
 * Export a result to a file: CSV / JSON / Excel / SQL INSERT / Markdown, the loaded rows or all
 * rows. "All rows" is streamed by the backend straight to the file, with progress and Cancel: a
 * table is read again under the data view's filter on a data session; a query result is written
 * from the exact rows the backend kept for it, without running its statement again. The save
 * dialog is opened by the backend, which only writes the file picked there.
 */
export function ResultExportDialog({ request, onClose }: Props) {
  const [opts, setOpts] = useState<ExportOptions>(() => defaultOptions(request.tableName));
  const [scope, setScope] = useState<Scope>(request.all?.kind === 'query' ? 'all' : 'loaded');
  const [phase, setPhase] = useState<Phase>({ kind: 'form' });
  const [error, setError] = useState<string | null>(null);
  const run = useRef(0);

  const set = (change: Partial<ExportOptions>) => setOpts((o) => ({ ...o, ...change }));
  const busy = phase.kind === 'running';
  const tableMissing = opts.format === 'sql' && opts.table_name.trim() === '';
  const canExport = !busy && !tableMissing && (scope === 'loaded' || request.all !== null);
  const partial = request.partialBinaries;

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    if (!canExport) return;
    const my = ++run.current;
    setError(null);
    let path: string | null;
    try {
      path = await api.exportPickPath(request.baseName, opts.format);
    } catch (err) {
      if (my === run.current) setError(toAppError(err).message);
      return;
    }
    if (my !== run.current || !path) return;
    const all = scope === 'all' ? request.all : null;
    const stop: Stop =
      all?.kind === 'table'
        ? { kind: 'table', connectionId: all.connectionId, key: `export:${Date.now()}:${++exports}` }
        : all?.kind === 'query'
          ? { kind: 'stored', operation: crypto.randomUUID() }
          : null;
    setPhase({ kind: 'running', rows: 0, stop });
    const tabId = all?.kind === 'query' ? all.tabId : null;
    try {
      let summary: ExportSummary;
      if (!all) {
        summary = await api.exportRows(request.engine, request.columns, request.rows(), opts, path);
      } else {
        const channel = new Channel<ExportProgress>();
        channel.onmessage = (p) => {
          if (my === run.current) setPhase((ph) => (ph.kind === 'running' ? { ...ph, rows: p.rows } : ph));
        };
        if (all.kind === 'table' && stop?.kind === 'table') {
          summary = await api.exportTable(all.connectionId, all.obj, all.sort, all.filter, opts, path, stop.key, channel);
        } else if (all.kind === 'query' && stop?.kind === 'stored') {
          summary = await api.exportQueryResult(all.token, request.engine, opts, path, stop.operation, channel);
        } else {
          return;
        }
      }
      if (my === run.current) setPhase({ kind: 'done', summary });
    } catch (err) {
      const appError = toAppError(err);
      // A failed read may change the tab's transaction (PostgreSQL: aborted).
      const tx = asTxState(appError.tx_state);
      if (tabId && tx) useQueryRun.getState().setTxState(tabId, tx);
      if (my !== run.current) return;
      if (isCancelled(appError)) {
        setPhase({ kind: 'cancelled' });
      } else {
        setError(appError.message);
        setPhase({ kind: 'form' });
      }
    }
  };

  const cancel = () => {
    if (phase.kind !== 'running' || !phase.stop) return;
    // The backend stops the statement and removes the half-written file.
    const stop = phase.stop;
    if (stop.kind === 'table') api.tableDataCancel(stop.connectionId, stop.key).catch(() => undefined);
    else api.exportQueryResultCancel(stop.operation).catch(() => undefined);
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
              {s.partial_binaries > 0 ? (
                <p className="hint">
                  {plural(s.partial_binaries, 'binary value was', 'binary values were')} only partially loaded and
                  written as {opts.format === 'sql' || opts.format === 'json' ? 'NULL' : 'empty cells'}. Export all rows to
                  get them whole.
                </p>
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
              <label className="check">
                <input
                  type="checkbox"
                  checked={opts.formula_guard ?? false}
                  onChange={(e) => set({ formula_guard: e.target.checked })}
                />
                Protect against formula injection
              </label>
              <p className="hint">
                NULL is an empty field; an empty string is written as "". With protection on, text starting with =, +, -,
                @, a tab or a carriage return gets a leading ' so spreadsheets show it as text.
              </p>
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
            {request.all?.kind === 'query' ? (
              <label className="check">
                <input type="radio" name="rx-scope" checked readOnly />
                Exact stored result ({fmt(request.loadedCount)} rows)
              </label>
            ) : (
              <>
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
              </>
            )}
            {request.all === null && request.allUnavailable ? <p className="hint">{request.allUnavailable}</p> : null}
            {scope === 'loaded' && request.loadedNote ? <p className="hint">{request.loadedNote}</p> : null}
            {scope === 'loaded' && partial > 0 ? (
              <p className="hint" data-testid="rx-partial-note">
                {plural(partial, 'binary value is', 'binary values are')} only partially loaded —{' '}
                {request.all !== null
                  ? 'choose All rows to export them fully.'
                  : `they are exported as ${opts.format === 'sql' || opts.format === 'json' ? 'NULL' : 'empty cells'}.`}
              </p>
            ) : null}
            {scope === 'all' && request.all?.kind === 'query' ? (
              <p className="hint">Exports the exact rows retained for this result without running the statement again.</p>
            ) : null}
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
            <button type="button" className="btn" onClick={cancel} disabled={!phase.stop}>
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
