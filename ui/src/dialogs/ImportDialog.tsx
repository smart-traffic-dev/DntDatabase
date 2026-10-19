import { open } from '@tauri-apps/plugin-dialog';
import { useEffect, useRef, useState, type FormEvent } from 'react';
import { api, toAppError } from '../api';
import type { ConflictPolicy, DbKind, ImportPreview } from '../bindings';
import { useDialogs } from '../store/dialogs';
import { useTabs } from '../store/tabs';
import { useTree } from '../store/tree';
import { Modal } from './Modal';

interface Props {
  onClose(): void;
}

const POLICIES: { value: ConflictPolicy; label: string; help: string }[] = [
  { value: 'skip', label: 'Skip', help: 'Keep the stored connection; do not import the entry.' },
  { value: 'overwrite', label: 'Overwrite', help: 'Replace the stored connection that has the same id.' },
  { value: 'copy', label: 'Copy', help: 'Import as a new connection next to the stored one.' },
];

const CONFLICT_LABEL = { same_id: 'Same id', same_name: 'Same name' } as const;

const chip = (kind: DbKind) => (kind === 'postgres' ? 'PG' : 'MY');

/** Import dialog for `.dntconn` and Navicat `.ncx` files. */
export function ImportDialog({ onClose }: Props) {
  const [path, setPath] = useState<string | null>(null);
  const [needsPassphrase, setNeedsPassphrase] = useState(false);
  const [passphrase, setPassphrase] = useState('');
  /** The passphrase the preview was built with (null: without passwords). */
  const [used, setUsed] = useState<string | null>(null);
  const [preview, setPreview] = useState<ImportPreview | null>(null);
  const [policy, setPolicy] = useState<ConflictPolicy>('skip');
  const [busy, setBusy] = useState(false);
  const [applying, setApplying] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [passError, setPassError] = useState<string | null>(null);
  // Results of calls that finish after the dialog closed or a newer call started are dropped.
  const token = useRef(0);
  useEffect(() => {
    const t = token;
    return () => {
      t.current++;
    };
  }, []);

  const fail = (e: unknown) => {
    const err = toAppError(e);
    if (err.field === 'passphrase') setPassError(err.message);
    else setError(err.message);
  };

  const loadPreview = async (file: string, pass: string | null, my: number) => {
    setBusy(true);
    try {
      const result = await api.previewImport(file, pass);
      if (my !== token.current) return;
      setUsed(pass);
      setPreview(result);
    } catch (e) {
      if (my !== token.current) return;
      fail(e);
    } finally {
      if (my === token.current) setBusy(false);
    }
  };

  const choose = async () => {
    const my = ++token.current;
    setError(null);
    setPassError(null);
    let file: string | string[] | null;
    try {
      file = await open({
        multiple: false,
        directory: false,
        filters: [
          { name: 'DntDatabase connections', extensions: ['dntconn'] },
          { name: 'Navicat connections', extensions: ['ncx'] },
          { name: 'All files', extensions: ['*'] },
        ],
      });
    } catch (e) {
      if (my === token.current) fail(e);
      return;
    }
    if (!file || Array.isArray(file) || my !== token.current) return;
    setPath(file);
    setNeedsPassphrase(false);
    setPassError(null);
    setPreview(null);
    setUsed(null);
    setPassphrase('');
    setBusy(true);
    try {
      const needs = await api.importNeedsPassphrase(file);
      if (my !== token.current) return;
      setNeedsPassphrase(needs);
      if (!needs) await loadPreview(file, null, my);
    } catch (e) {
      if (my !== token.current) return;
      fail(e);
    } finally {
      if (my === token.current) setBusy(false);
    }
  };

  const withPassphrase = (e: FormEvent) => {
    e.preventDefault();
    if (!path || !passphrase || busy) return;
    setPassError(null);
    setError(null);
    void loadPreview(path, passphrase, ++token.current);
  };

  const withoutPasswords = () => {
    if (!path || busy) return;
    setPassError(null);
    setError(null);
    setPassphrase('');
    void loadPreview(path, null, ++token.current);
  };

  const apply = async () => {
    if (!path || !preview || busy) return;
    const my = ++token.current;
    setBusy(true);
    setApplying(true);
    setError(null);
    try {
      const r = await api.applyImport(path, used, policy);
      // The import happened on the backend: always surface it, even if the dialog went away.
      useDialogs.getState().setNotice(`Imported ${r.imported} · Overwritten ${r.overwritten} · Skipped ${r.skipped}`);
      // The backend closed the replaced connections: drop what the UI still shows of them.
      for (const id of r.overwritten_ids) {
        useTree.getState().forgetConnection(id);
        useTabs.getState().closeConnection(id);
      }
      await useTree.getState().reloadStored();
      if (my !== token.current) return;
      onClose();
    } catch (e) {
      if (my !== token.current) return;
      setError(toAppError(e).message);
      setBusy(false);
      setApplying(false);
    }
  };

  const close = () => {
    // The dialog stays until a running import has finished.
    if (applying) return;
    token.current++;
    setPassphrase('');
    setUsed(null);
    onClose();
  };

  const showPassphrase = needsPassphrase && !preview;

  return (
    <Modal title="Import connections" onClose={close} className="wide">
      <div className="modal-body stack">
        <div className="field">
          <label htmlFor="im-file">File</label>
          <div className="row">
            <input id="im-file" readOnly value={path ?? ''} placeholder="No file selected" />
            <button type="button" className="btn" disabled={busy} onClick={() => void choose()}>
              Choose file…
            </button>
          </div>
        </div>

        {showPassphrase && (
          <form className="stack" onSubmit={withPassphrase}>
            <div className="field">
              <label htmlFor="im-pass">Passphrase</label>
              <input
                id="im-pass"
                type="password"
                autoComplete="off"
                value={passphrase}
                aria-invalid={passError !== null}
                aria-describedby={passError ? 'im-pass-error' : undefined}
                onChange={(e) => setPassphrase(e.target.value)}
              />
              {passError && (
                <p id="im-pass-error" role="alert" className="field-error">
                  {passError}
                </p>
              )}
            </div>
            <p className="hint">This file contains saved passwords protected by a passphrase.</p>
            <div className="row">
              <button type="submit" className="btn primary" disabled={busy || !passphrase}>
                Preview
              </button>
              <button type="button" className="btn" disabled={busy} onClick={withoutPasswords}>
                Import without passwords
              </button>
            </div>
          </form>
        )}

        {preview && (
          <>
            <table className="import-table">
              <thead>
                <tr>
                  <th>Name</th>
                  <th>Engine</th>
                  <th>Host</th>
                  <th>Group</th>
                  <th>Conflict</th>
                  <th>Warnings</th>
                </tr>
              </thead>
              <tbody>
                {preview.rows.map((r) => (
                  <tr key={r.id}>
                    <td>{r.name}</td>
                    <td>
                      <span className={`chip chip-${chip(r.kind)}`}>{chip(r.kind)}</span>
                    </td>
                    <td>{r.host}</td>
                    <td>{r.group_path}</td>
                    <td>{r.conflict ? CONFLICT_LABEL[r.conflict] : '—'}</td>
                    <td>
                      {r.warnings.length > 0 ? (
                        <ul className="import-warnings">
                          {r.warnings.map((w) => (
                            <li key={w}>{w}</li>
                          ))}
                        </ul>
                      ) : (
                        '—'
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
            {preview.rows.length === 0 && <p className="hint">Nothing to import.</p>}

            {preview.skipped.length > 0 && (
              <section aria-label="Skipped entries">
                <h3 className="import-heading">Skipped</h3>
                <ul className="import-skipped">
                  {preview.skipped.map((s, i) => (
                    <li key={`${s.name}:${i}`}>
                      <strong>{s.name}</strong> — {s.reason}
                    </li>
                  ))}
                </ul>
              </section>
            )}

            <fieldset className="import-policy">
              <legend>When a connection already exists</legend>
              {POLICIES.map((p) => (
                <label key={p.value} className="check">
                  <input
                    type="radio"
                    name="import-policy"
                    value={p.value}
                    checked={policy === p.value}
                    onChange={() => setPolicy(p.value)}
                  />
                  <span>
                    <strong>{p.label}</strong> <span className="hint">{p.help}</span>
                  </span>
                </label>
              ))}
            </fieldset>
          </>
        )}

        {error && (
          <p role="alert" className="field-error">
            {error}
          </p>
        )}
        {busy && (
          <p role="status" className="hint">
            Working…
          </p>
        )}
      </div>
      <footer className="modal-foot">
        <button type="button" className="btn" disabled={applying} onClick={close}>
          Cancel
        </button>
        <button
          type="button"
          className="btn primary"
          disabled={busy || !preview || preview.rows.length === 0}
          onClick={() => void apply()}
        >
          Import
        </button>
      </footer>
    </Modal>
  );
}
