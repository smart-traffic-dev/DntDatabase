import { useEffect, useRef, useState, type UIEvent } from 'react';
import { api, toAppError } from '../../api';
import type { RestorePreview, RestoreRequest, ScanStats } from '../../bindings';
import { EnvTag } from '../../components/EnvTag';
import { showJobs } from '../../components/windowActions';
import { Modal } from '../../dialogs/Modal';
import { formatBytes } from '../../jobs/jobFormat';
import { useConnections } from '../../store/connections';
import { useJobs } from '../../store/jobs';
import { PATH_LABEL, restoreMessage } from './backupFormat';
import './backup.css';

interface Props {
  connectionId: string;
  /** The target database the dialog starts with (the tree's database or schema node). */
  database?: string;
  /** "Run again" (R5): pre-filled, analyzed and confirmed again. */
  params?: RestoreRequest;
  onClose(): void;
}

interface Failure {
  message: string;
  /** psql / pg_restore missing (or another refusal nothing can be started from). */
  unsupported: boolean;
}

interface Confirmation {
  typed: string;
  understood: boolean;
}

const NO_CONFIRMATION: Confirmation = { typed: '', understood: false };

/** The confirmation the preview asks for is complete (R32). */
function confirmationDone(preview: RestorePreview, c: Confirmation): boolean {
  switch (preview.confirm.kind) {
    case 'none':
      return true;
    case 'click':
      return c.understood;
    case 'type_name':
      return c.typed === preview.confirm.name;
  }
}

const STAT_ROWS: { key: keyof ScanStats; label: string }[] = [
  { key: 'statements', label: 'Statements' },
  { key: 'ddl', label: 'DDL' },
  { key: 'writes', label: 'Writes' },
  { key: 'drops', label: 'Drops' },
  { key: 'reads', label: 'Reads' },
  { key: 'transaction', label: 'Transaction control' },
  { key: 'other', label: 'Other' },
  { key: 'copy_blocks', label: 'COPY blocks' },
  { key: 'client_commands', label: 'psql commands' },
];

const TOC_ROW = 20;
const TOC_HEIGHT = 240;
const TOC_OVERSCAN = 10;

/** The pg_restore TOC: a fixed-height list that renders only the rows in view. */
function TocList({ entries }: { entries: string[] }) {
  const [top, setTop] = useState(0);
  const first = Math.max(0, Math.floor(top / TOC_ROW) - TOC_OVERSCAN);
  const last = Math.min(entries.length, Math.ceil((top + TOC_HEIGHT) / TOC_ROW) + TOC_OVERSCAN);
  return (
    <ul
      className="backup-toc mono"
      aria-label="Archive contents"
      tabIndex={0}
      style={{ height: TOC_HEIGHT, paddingTop: first * TOC_ROW, paddingBottom: (entries.length - last) * TOC_ROW }}
      onScroll={(e: UIEvent<HTMLUListElement>) => setTop(e.currentTarget.scrollTop)}
    >
      {entries.slice(first, last).map((line, i) => (
        <li key={first + i} style={{ height: TOC_ROW }}>
          {line}
        </li>
      ))}
    </ul>
  );
}

/**
 * The restore dialog (spec §7.4): file, target database, **Analyze** (the backend picks the path by
 * content and stores a single-use plan), then the preview — path, statistics or TOC, command line,
 * transaction, warnings — and the confirmation of R32 before **Start restore**.
 */
export function RestoreDialog({ connectionId, database, params, onClose }: Props) {
  const cfg = useConnections((s) => s.summaries.get(connectionId)?.config);
  const pg = cfg?.kind === 'postgres';
  const readOnly = !!cfg?.read_only;
  const production = cfg?.env_tag === 'production';

  const [databases, setDatabases] = useState<string[]>([]);
  const [target, setTarget] = useState(params?.database ?? database ?? '');
  const [path, setPath] = useState<string | null>(params?.path ?? null);
  const [singleTransaction, setSingleTransaction] = useState(params?.single_transaction ?? true);
  const [clean, setClean] = useState(params?.clean ?? false);
  const [noOwner, setNoOwner] = useState(params?.no_owner ?? false);
  // The analysis together with the request it answered: the confirmation names what was analyzed.
  const [analyzed, setAnalyzed] = useState<{ request: RestoreRequest; preview: RestorePreview } | null>(null);
  const preview = analyzed?.preview ?? null;
  const [failure, setFailure] = useState<Failure | null>(null);
  const [analyzing, setAnalyzing] = useState(false);
  const [starting, setStarting] = useState(false);
  const [picking, setPicking] = useState(false);
  const [confirmation, setConfirmation] = useState<Confirmation>(NO_CONFIRMATION);
  const [databasesError, setDatabasesError] = useState<string | null>(null);
  // Bumped by every analysis and every input change: an answer for an older one is dropped.
  const analysisSeq = useRef(0);

  useEffect(() => {
    let live = true;
    api.listDatabases(connectionId, false).then(
      (result) => {
        if (!live) return;
        const names = result.map((d) => d.name);
        setDatabases(names);
        setTarget((current) => current || names[0] || '');
      },
      // The database stays as given (the analysis reports a wrong one); say why the list is short.
      (e) => live && setDatabasesError(`The databases could not be listed: ${toAppError(e).message}`),
    );
    return () => {
      live = false;
    };
  }, [connectionId]);

  /** Every input change drops the analysis: its plan was made for the old inputs. */
  const changed = () => {
    analysisSeq.current += 1;
    // An analysis in flight is abandoned (its answer is dropped).
    setAnalyzing(false);
    setAnalyzed(null);
    setFailure(null);
    setConfirmation(NO_CONFIRMATION);
  };

  const browse = async () => {
    setPicking(true);
    try {
      const picked = await api.restorePickFile();
      if (picked) {
        setPath(picked);
        changed();
      }
    } catch (e) {
      setFailure({ message: restoreMessage(toAppError(e)), unsupported: false });
    } finally {
      setPicking(false);
    }
  };

  const request = (): RestoreRequest | null =>
    path && target
      ? { connection_id: connectionId, database: target, path, single_transaction: singleTransaction, clean, no_owner: noOwner }
      : null;

  const analyze = async () => {
    const req = request();
    if (!req) return;
    changed();
    const seq = analysisSeq.current;
    setAnalyzing(true);
    try {
      const result = await api.restorePreview(req);
      if (seq === analysisSeq.current) setAnalyzed({ request: req, preview: result });
    } catch (e) {
      const err = toAppError(e);
      if (seq === analysisSeq.current) setFailure({ message: restoreMessage(err), unsupported: err.kind === 'unsupported' });
    } finally {
      if (seq === analysisSeq.current) setAnalyzing(false);
    }
  };

  const start = async () => {
    if (!preview) return;
    setStarting(true);
    setFailure(null);
    try {
      await api.restoreStart(preview.plan_id, true, preview.confirm.kind === 'type_name' ? confirmation.typed : null);
      setStarting(false);
      onClose();
      showJobs();
      void useJobs.getState().load();
    } catch (e) {
      // The plan is single use: whatever failed, the file must be analyzed again.
      const err = toAppError(e);
      changed();
      setFailure({ message: restoreMessage(err), unsupported: err.kind === 'unsupported' });
      setStarting(false);
    }
  };

  const close = () => {
    if (!starting) onClose();
  };

  // While a file is analyzed or a restore starts, the inputs stay as they were sent.
  const busy = analyzing || starting || picking;
  const analyzedDatabase = analyzed?.request.database ?? target;
  const canAnalyze = !!path && !!target && !busy && !readOnly;
  const canStart = !!preview && !starting && !analyzing && !readOnly && confirmationDone(preview, confirmation);
  const options = databases.includes(target) || !target ? databases : [target, ...databases];

  return (
    <Modal
      title="Restore"
      badge={production ? <EnvTag env="production" variant="chip" /> : undefined}
      onClose={close}
      className={`backup-dialog${production ? ' env-production' : ''}`}
      testId="restore-dialog"
    >
      <div className="modal-body stack backup-body">
        <section className="stack" aria-label="File">
          <h3 className="backup-heading">File</h3>
          <div className="backup-row">
            {path ? (
              <span className="backup-path mono" title={path}>
                {path}
              </span>
            ) : (
              <span className="hint">No file chosen.</span>
            )}
            <span className="spacer" />
            <button type="button" className="btn" disabled={busy} onClick={() => void browse()}>
              Browse…
            </button>
          </div>
          <p className="hint">The way the file is restored is chosen from its content, not its extension.</p>
        </section>

        <section className="stack" aria-label="Target">
          <h3 className="backup-heading">Target</h3>
          <dl className="kv">
            <dt>Connection</dt>
            <dd>{cfg?.name ?? connectionId}</dd>
          </dl>
          <label className="field backup-database">
            <span className="field-label">Database</span>
            <select
              value={target}
              disabled={busy}
              onChange={(e) => {
                setTarget(e.target.value);
                changed();
              }}
            >
              {options.map((d) => (
                <option key={d} value={d}>
                  {d}
                </option>
              ))}
            </select>
          </label>
          {databasesError && <p className="field-error">{databasesError}</p>}
          {pg && (
            <div className="backup-options">
              <label className="check">
                <input
                  type="checkbox"
                  disabled={busy}
                  checked={singleTransaction}
                  onChange={(e) => {
                    setSingleTransaction(e.target.checked);
                    changed();
                  }}
                />
                <span>Single transaction</span>
              </label>
              <label className="check">
                <input
                  type="checkbox"
                  disabled={busy}
                  checked={clean}
                  onChange={(e) => {
                    setClean(e.target.checked);
                    changed();
                  }}
                />
                <span>Clean (drop objects first)</span>
              </label>
              <label className="check">
                <input
                  type="checkbox"
                  disabled={busy}
                  checked={noOwner}
                  onChange={(e) => {
                    setNoOwner(e.target.checked);
                    changed();
                  }}
                />
                <span>No owner</span>
              </label>
            </div>
          )}
          {pg && <p className="hint">Single transaction applies to psql and pg_restore; the built-in runner decides from the file. Clean and No owner apply to pg_restore.</p>}
          {readOnly && <p className="backup-note">This connection is read-only: restoring is blocked.</p>}
          <div className="backup-row">
            <button type="button" className="btn" disabled={!canAnalyze} onClick={() => void analyze()}>
              Analyze
            </button>
            {analyzing && (
              <span role="status" className="hint">
                Analyzing the file…
              </span>
            )}
          </div>
        </section>

        {failure && (
          <p role="alert" className="field-error">
            {failure.message}
          </p>
        )}

        {preview && (
          <section className="stack" aria-label="Analysis" data-testid="restore-preview">
            <div className="backup-row">
              <span className="backup-chip" data-testid="restore-path">
                {PATH_LABEL[preview.path]}
              </span>
              {preview.tool && <span className="hint">{preview.tool.version}</span>}
            </div>
            <dl className="kv">
              <dt>File</dt>
              <dd>
                {preview.file.name} · {formatBytes(preview.file.size)} · SHA-256 <span className="mono">{preview.file.sha256.slice(0, 12)}</span>
                {preview.compressed ? ' · gzip' : ''}
              </dd>
              <dt>Transaction</dt>
              <dd>{preview.transactional ? 'One transaction: a failure restores nothing.' : 'Not in one transaction.'}</dd>
            </dl>

            {preview.stats && (
              <>
                {preview.stats.app_dump && <p className="hint">Written by the DntDatabase dump.</p>}
                <table className="backup-stats" aria-label="Statements">
                  <tbody>
                    {STAT_ROWS.map((r) => ({ ...r, value: Number(preview.stats?.[r.key] ?? 0) }))
                      // The total and the drops always; the other counts when the file has any.
                      .filter((r) => r.key === 'statements' || r.key === 'drops' || r.value > 0)
                      .map((r) => (
                        <tr key={r.key}>
                          <th scope="row">{r.label}</th>
                          <td>{r.value.toLocaleString('en-US')}</td>
                        </tr>
                      ))}
                  </tbody>
                </table>
              </>
            )}

            {preview.path === 'pg_restore' && (
              <>
                <h3 className="backup-heading">Archive contents</h3>
                {preview.toc.length > 0 ? <TocList entries={preview.toc} /> : <p className="hint">The archive lists no entries.</p>}
                {preview.toc_truncated && <p className="hint">Showing the first {preview.toc.length} entries.</p>}
              </>
            )}

            {preview.command && (
              <>
                <h3 className="backup-heading">Command line</h3>
                <pre className="backup-command" data-testid="restore-command">
                  {preview.command}
                </pre>
              </>
            )}

            {preview.warnings.length > 0 && (
              <ul className="backup-warnings">
                {preview.warnings.map((w) => (
                  <li key={w}>{w}</li>
                ))}
              </ul>
            )}

            {preview.confirm.kind === 'click' && (
              <label className="check">
                <input type="checkbox" checked={confirmation.understood} onChange={(e) => setConfirmation({ ...confirmation, understood: e.target.checked })} />
                <span>I understand that the restore changes {analyzedDatabase}</span>
              </label>
            )}
            {preview.confirm.kind === 'type_name' && (
              <label className="field">
                <span className="field-label">
                  Type <code className="run-confirm-name">{preview.confirm.name}</code> to confirm
                </span>
                <input
                  type="text"
                  className="run-confirm-input"
                  autoComplete="off"
                  autoCorrect="off"
                  spellCheck={false}
                  value={confirmation.typed}
                  onChange={(e) => setConfirmation({ ...confirmation, typed: e.target.value })}
                />
              </label>
            )}
            <p className="hint">This analysis is valid for {Math.max(1, Math.round(preview.expires_in_seconds / 60))} min.</p>
          </section>
        )}
      </div>
      <footer className="modal-foot">
        <button type="button" className="btn" disabled={starting} onClick={close}>
          Cancel
        </button>
        <span className="spacer" />
        {!failure?.unsupported && (
          <button type="button" className={`btn ${production ? 'danger' : 'primary'}`} disabled={!canStart} onClick={() => void start()}>
            Start restore
          </button>
        )}
      </footer>
    </Modal>
  );
}
