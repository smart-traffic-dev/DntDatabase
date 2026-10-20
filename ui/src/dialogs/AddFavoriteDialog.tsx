import { useId, useMemo, useState, type FormEvent } from 'react';
import { toAppError } from '../api';
import type { Favorite } from '../bindings';
import { useConnections } from '../store/connections';
import { folderPaths, useFavorites } from '../store/favorites';
import { Modal } from './Modal';

/** A folder path: pick an existing one from the list or type a new one (`Reports/Daily`). Blank: the top level. */
export function FolderField({ id, value, onChange }: { id: string; value: string; onChange(v: string): void }) {
  const listId = useId();
  const favorites = useFavorites((s) => s.favorites);
  const folders = useMemo(() => folderPaths(favorites), [favorites]);
  return (
    <div className="field">
      <label htmlFor={id}>Folder</label>
      <input
        id={id}
        list={listId}
        value={value}
        placeholder="(top level)"
        autoComplete="off"
        onChange={(e) => onChange(e.target.value)}
      />
      <datalist id={listId}>
        {folders.map((f) => (
          <option key={f} value={f} />
        ))}
      </datalist>
      <p className="hint">Choose a folder or type a new one; use / for subfolders.</p>
    </div>
  );
}

interface AddProps {
  sql: string;
  name: string;
  connectionId: string | null;
  database: string | null;
  fromSelection: boolean;
  onClose(saved: Favorite | null): void;
}

/** "Add to favorites": name, folder, and whether the favorite keeps the tab's connection. */
export function AddFavoriteDialog({ sql, name: initialName, connectionId, database, fromSelection, onClose }: AddProps) {
  const connectionName = useConnections((s) => (connectionId ? s.summaries.get(connectionId)?.config.name : undefined));
  const [name, setName] = useState(initialName);
  const [folder, setFolder] = useState('');
  const [keep, setKeep] = useState(!!connectionName);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    if (!name.trim()) return;
    setBusy(true);
    setError(null);
    try {
      const saved = await useFavorites.getState().create({
        name: name.trim(),
        folder: folder.trim() || null,
        sql,
        connection_id: keep ? connectionId : null,
        database: keep ? database : null,
      });
      onClose(saved);
    } catch (err) {
      setError(toAppError(err).message);
      setBusy(false);
    }
  };

  const target = connectionName ? (database ? `${connectionName} / ${database}` : connectionName) : null;
  return (
    <Modal title="Add to favorites" onClose={() => onClose(null)} className="narrow" testId="add-favorite-dialog">
      <form onSubmit={(e) => void submit(e)}>
        <div className="modal-body stack">
          <div className="field">
            <label htmlFor="fav-name">Name</label>
            <input id="fav-name" value={name} onChange={(e) => setName(e.target.value)} />
          </div>
          <FolderField id="fav-folder" value={folder} onChange={setFolder} />
          <label className="check">
            <input type="checkbox" checked={keep} disabled={!connectionName} onChange={(e) => setKeep(e.target.checked)} />
            <span>{target ? `Keep connection (${target})` : 'Keep connection (the tab has none)'}</span>
          </label>
          <p className="hint">{fromSelection ? 'Saves the selected text.' : 'Saves the whole query.'}</p>
          {error && (
            <p role="alert" className="field-error">
              {error}
            </p>
          )}
        </div>
        <footer className="modal-foot">
          <button type="button" className="btn" onClick={() => onClose(null)}>
            Cancel
          </button>
          <button type="submit" className="btn primary" disabled={busy || !name.trim()}>
            Add
          </button>
        </footer>
      </form>
    </Modal>
  );
}

/** "Move to folder" of one favorite. */
export function MoveFavoriteDialog({ favorite, onClose }: { favorite: Favorite; onClose(): void }) {
  const [folder, setFolder] = useState(favorite.folder ?? '');
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await useFavorites.getState().move(favorite.id, folder.trim() || null);
      onClose();
    } catch (err) {
      setError(toAppError(err).message);
      setBusy(false);
    }
  };

  return (
    <Modal title={`Move "${favorite.name}"`} onClose={onClose} className="narrow" testId="move-favorite-dialog">
      <form onSubmit={(e) => void submit(e)}>
        <div className="modal-body stack">
          <FolderField id="fav-move-folder" value={folder} onChange={setFolder} />
          {error && (
            <p role="alert" className="field-error">
              {error}
            </p>
          )}
        </div>
        <footer className="modal-foot">
          <button type="button" className="btn" onClick={onClose}>
            Cancel
          </button>
          <button type="submit" className="btn primary" disabled={busy}>
            Move
          </button>
        </footer>
      </form>
    </Modal>
  );
}
