import { save } from '@tauri-apps/plugin-dialog';
import { useMemo, useRef, useState, type FormEvent, type ReactElement } from 'react';
import { api, toAppError } from '../api';
import type { ConnectionSummary, Group, NodeId } from '../bindings';
import { useConnections } from '../store/connections';
import { Modal } from './Modal';

export const MIN_PASSPHRASE = 8;

interface Props {
  /** Nodes that start out checked (a group checks its whole subtree). */
  preselected?: NodeId[];
  onClose(): void;
}

interface Model {
  parentOf: Map<string, string | null>;
  /** Child ids (groups and connections) per group id. */
  childrenOf: Map<string, string[]>;
  groupIds: Set<string>;
  roots: string[];
  names: Map<string, string>;
}

function buildModel(groups: Group[], connections: ConnectionSummary[]): Model {
  const parentOf = new Map<string, string | null>();
  const childrenOf = new Map<string, string[]>();
  const names = new Map<string, string>();
  const groupIds = new Set(groups.map((g) => g.id));
  const roots: string[] = [];
  const add = (id: string, parent: string | null, name: string) => {
    // A dangling parent is shown at the top level.
    const p = parent && groupIds.has(parent) ? parent : null;
    parentOf.set(id, p);
    names.set(id, name);
    if (p) childrenOf.set(p, [...(childrenOf.get(p) ?? []), id]);
    else roots.push(id);
  };
  for (const g of groups) add(g.id, g.parent_id, g.name);
  for (const c of connections) add(c.config.id, c.config.group_id, c.config.name);
  return { parentOf, childrenOf, groupIds, roots, names };
}

function subtree(m: Model, id: string, out: string[] = []): string[] {
  out.push(id);
  for (const child of m.childrenOf.get(id) ?? []) subtree(m, child, out);
  return out;
}

/** Export dialog: pick groups/connections, optionally protect saved passwords with a passphrase. */
export function ExportDialog({ preselected = [], onClose }: Props) {
  const groups = useConnections((s) => s.groups);
  const summaries = useConnections((s) => s.summaries);
  const model = useMemo(() => buildModel(groups, [...summaries.values()]), [groups, summaries]);

  const [checked, setChecked] = useState<Set<string>>(() => {
    const start = new Set<string>();
    for (const n of preselected) {
      if (model.names.has(n.id)) subtree(model, n.id).forEach((id) => start.add(id));
    }
    return start;
  });
  const [includePasswords, setIncludePasswords] = useState(false);
  const [passphrase, setPassphrase] = useState('');
  const [confirm, setConfirm] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const run = useRef(0);

  const toggle = (id: string, on: boolean) => {
    setChecked((prev) => {
      const next = new Set(prev);
      if (on) {
        subtree(model, id).forEach((x) => next.add(x));
        // A parent whose children are all checked is checked too.
        let p = model.parentOf.get(id) ?? null;
        while (p && (model.childrenOf.get(p) ?? []).every((c) => next.has(c))) {
          next.add(p);
          p = model.parentOf.get(p) ?? null;
        }
      } else {
        subtree(model, id).forEach((x) => next.delete(x));
        for (let p = model.parentOf.get(id) ?? null; p; p = model.parentOf.get(p) ?? null) next.delete(p);
      }
      return next;
    });
  };

  /** Top-most checked groups, plus checked connections that no checked group covers. */
  const selectedNodes = (): NodeId[] => {
    const nodes: NodeId[] = [];
    for (const id of checked) {
      const parent = model.parentOf.get(id) ?? null;
      if (parent && checked.has(parent)) continue;
      nodes.push({ type: model.groupIds.has(id) ? 'group' : 'connection', id });
    }
    return nodes;
  };

  const connectionCount = [...checked].filter((id) => !model.groupIds.has(id)).length;
  const tooShort = includePasswords && passphrase.length > 0 && passphrase.length < MIN_PASSPHRASE;
  const mismatch = includePasswords && confirm.length > 0 && passphrase !== confirm;
  const passphraseOk = !includePasswords || (passphrase.length >= MIN_PASSPHRASE && passphrase === confirm);
  const canExport = checked.size > 0 && passphraseOk && !busy;

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    if (!canExport) return;
    const my = ++run.current;
    setError(null);
    try {
      const day = new Date().toISOString().slice(0, 10);
      const path = await save({
        defaultPath: `connections-${day}.dntconn`,
        filters: [{ name: 'DntDatabase connections', extensions: ['dntconn'] }],
      });
      if (!path || my !== run.current) return;
      setBusy(true);
      await api.exportConnections(selectedNodes(), includePasswords ? passphrase : null, path);
      if (my !== run.current) return;
      onClose();
    } catch (err) {
      if (my !== run.current) return;
      setError(toAppError(err).message);
      setBusy(false);
    }
  };

  const close = () => {
    run.current++;
    setPassphrase('');
    setConfirm('');
    onClose();
  };

  const renderNode = (id: string): ReactElement => {
    const isGroup = model.groupIds.has(id);
    const name = model.names.get(id) ?? id;
    return (
      <li key={id}>
        <label className="check export-node">
          <input type="checkbox" checked={checked.has(id)} onChange={(e) => toggle(id, e.target.checked)} />
          <span>{name}</span>
          {isGroup && <span className="hint">group</span>}
        </label>
        {isGroup && (model.childrenOf.get(id)?.length ?? 0) > 0 && (
          <ul className="export-tree">{(model.childrenOf.get(id) ?? []).map(renderNode)}</ul>
        )}
      </li>
    );
  };

  return (
    <Modal title="Export connections" onClose={close}>
      <form onSubmit={(e) => void submit(e)}>
        <div className="modal-body stack">
          {model.roots.length === 0 ? (
            <p className="hint">There are no connections to export.</p>
          ) : (
            <ul className="export-tree root" aria-label="Connections to export">
              {model.roots.map(renderNode)}
            </ul>
          )}
          <div className="field">
            <label className="check">
              <input
                type="checkbox"
                checked={includePasswords}
                onChange={(e) => setIncludePasswords(e.target.checked)}
              />
              Include passwords
            </label>
          </div>
          {includePasswords && (
            <>
              <div className="field">
                <label htmlFor="ex-pass">Passphrase</label>
                <input
                  id="ex-pass"
                  type="password"
                  autoComplete="new-password"
                  value={passphrase}
                  aria-invalid={tooShort}
                  onChange={(e) => setPassphrase(e.target.value)}
                />
                {tooShort && (
                  <p role="alert" className="field-error">
                    Passphrase must be at least {MIN_PASSPHRASE} characters.
                  </p>
                )}
              </div>
              <div className="field">
                <label htmlFor="ex-confirm">Confirm passphrase</label>
                <input
                  id="ex-confirm"
                  type="password"
                  autoComplete="new-password"
                  value={confirm}
                  aria-invalid={mismatch}
                  onChange={(e) => setConfirm(e.target.value)}
                />
                {mismatch && (
                  <p role="alert" className="field-error">
                    Passphrases do not match.
                  </p>
                )}
              </div>
            </>
          )}
          <p className="hint">SSH private keys are exported as file paths only.</p>
          {error && (
            <p role="alert" className="field-error">
              {error}
            </p>
          )}
        </div>
        <footer className="modal-foot">
          <span className="hint spacer">{connectionCount} connection(s) selected</span>
          <button type="button" className="btn" onClick={close}>
            Cancel
          </button>
          <button type="submit" className="btn primary" disabled={!canExport}>
            Export
          </button>
        </footer>
      </form>
    </Modal>
  );
}
