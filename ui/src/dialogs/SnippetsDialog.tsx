import { useEffect, useState, type FormEvent } from 'react';
import { toAppError } from '../api';
import type { Snippet, SnippetInput } from '../bindings';
import { useSnippets } from '../store/snippets';
import { Modal } from './Modal';
import { ConfirmDialog } from './SimpleDialogs';
import './snippets.css';

const EMPTY: SnippetInput = { prefix: '', label: '', body: '', description: '' };

const toInput = (s: Snippet): SnippetInput => ({ prefix: s.prefix, label: s.label, body: s.body, description: s.description });

/** The snippets manager: the user's snippets on the left, the chosen (or new) one on the right. */
export function SnippetsDialog({ onClose }: { onClose(): void }) {
  const snippets = useSnippets((s) => s.snippets);
  const loadError = useSnippets((s) => s.error);
  const [selected, setSelected] = useState<string | null>(null);
  const [form, setForm] = useState<SnippetInput>(EMPTY);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState<Snippet | null>(null);

  useEffect(() => {
    void useSnippets.getState().load();
  }, []);

  const current = selected ? snippets.find((s) => s.id === selected) : undefined;

  const choose = (s: Snippet | null) => {
    setSelected(s?.id ?? null);
    setForm(s ? toInput(s) : EMPTY);
    setError(null);
  };

  const set = (field: keyof SnippetInput) => (value: string) => setForm((f) => ({ ...f, [field]: value }));

  const save = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const store = useSnippets.getState();
      const saved = current ? await store.update(current.id, form) : await store.create(form);
      choose(saved);
    } catch (err) {
      setError(toAppError(err).message);
    } finally {
      setBusy(false);
    }
  };

  const valid = form.prefix.trim() !== '' && form.label.trim() !== '' && form.body.trim() !== '';

  return (
    <>
      <Modal title="Snippets" onClose={onClose} className="wide snippets-dialog" testId="snippets-dialog" closable>
        <form className="snippets-body" onSubmit={(e) => void save(e)}>
          <div className="snippets-side">
            <button type="button" className="btn" onClick={() => choose(null)}>
              New snippet
            </button>
            <ul className="snippets-list" role="listbox" aria-label="Snippets">
              {snippets.map((s) => (
                <li
                  key={s.id}
                  role="option"
                  aria-selected={s.id === selected}
                  tabIndex={0}
                  className={`snippets-item${s.id === selected ? ' selected' : ''}`}
                  onClick={() => choose(s)}
                  onKeyDown={(e) => {
                    if (e.key === 'Enter' || e.key === ' ') {
                      e.preventDefault();
                      choose(s);
                    }
                  }}
                >
                  <span className="mono">{s.prefix}</span>
                  <span className="snippets-label">{s.label}</span>
                </li>
              ))}
              {snippets.length === 0 && <li className="snippets-empty">No snippets yet.</li>}
            </ul>
            {loadError && (
              <p role="alert" className="field-error">
                {loadError.message}
              </p>
            )}
          </div>
          <div className="snippets-form stack">
            <div className="snippets-row">
              <div className="field">
                <label htmlFor="snip-prefix">Prefix</label>
                <input id="snip-prefix" className="mono" value={form.prefix} onChange={(e) => set('prefix')(e.target.value)} />
              </div>
              <div className="field">
                <label htmlFor="snip-label">Label</label>
                <input id="snip-label" value={form.label} onChange={(e) => set('label')(e.target.value)} />
              </div>
            </div>
            <div className="field">
              <label htmlFor="snip-description">Description</label>
              <input id="snip-description" value={form.description} onChange={(e) => set('description')(e.target.value)} />
            </div>
            <div className="field">
              <label htmlFor="snip-body">Body</label>
              <textarea
                id="snip-body"
                className="snippets-text mono"
                rows={8}
                spellCheck={false}
                value={form.body}
                onChange={(e) => set('body')(e.target.value)}
              />
              <p className="hint" data-testid="snippet-help">
                Type the prefix in the editor to insert the body. <code>{'${1:placeholder}'}</code> is the first tab stop with
                default text, <code>{'${2}'}</code> the next one, <code>$0</code> where the cursor ends; write <code>\$</code> for a
                literal $.
              </p>
            </div>
            {error && (
              <p role="alert" className="field-error">
                {error}
              </p>
            )}
            <div className="snippets-actions">
              {current && (
                <button type="button" className="btn danger" onClick={() => setConfirmDelete(current)}>
                  Delete
                </button>
              )}
              <span className="spacer" />
              <button type="submit" className="btn primary" disabled={busy || !valid}>
                {current ? 'Save' : 'Add snippet'}
              </button>
            </div>
          </div>
        </form>
      </Modal>
      {confirmDelete && (
        <ConfirmDialog
          title="Delete snippet"
          message={`Delete the snippet "${confirmDelete.prefix}"?`}
          confirmLabel="Delete"
          onClose={() => setConfirmDelete(null)}
          onConfirm={async () => {
            await useSnippets.getState().remove(confirmDelete.id);
            setConfirmDelete(null);
            choose(null);
          }}
        />
      )}
    </>
  );
}
