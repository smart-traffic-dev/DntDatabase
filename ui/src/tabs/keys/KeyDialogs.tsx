import { useId, useState, type FormEvent } from 'react';
import { Modal } from '../../dialogs/Modal';
import type { NewKey, NewKeyType } from './keyEdits';

interface PromptProps {
  title: string;
  label: string;
  initial: string;
  submitLabel: string;
  /** The problem with `text`, or null when it can be submitted. */
  validate(text: string): string | null;
  onSubmit(text: string): void;
  onCancel(): void;
}

/** One text field and a submit button (`Set TTL…`, `Rename…`). */
export function KeyPromptDialog({ title, label, initial, submitLabel, validate, onSubmit, onCancel }: PromptProps) {
  const [text, setText] = useState(initial);
  const id = useId();
  const problem = validate(text);
  const submit = (e: FormEvent) => {
    e.preventDefault();
    if (!problem) onSubmit(text);
  };
  return (
    <Modal title={title} onClose={onCancel} className="size-s" testId="key-prompt">
      <form onSubmit={submit}>
        <div className="modal-body flow">
          <div className="field">
            <label htmlFor={id}>{label}</label>
            <input id={id} value={text} aria-invalid={problem ? true : undefined} onChange={(e) => setText(e.target.value)} />
            {problem && text !== initial ? <p className="field-error">{problem}</p> : null}
          </div>
        </div>
        <footer className="modal-foot">
          <button type="button" className="btn" onClick={onCancel}>
            Cancel
          </button>
          <button type="submit" className="btn primary" disabled={!!problem}>
            {submitLabel}
          </button>
        </footer>
      </form>
    </Modal>
  );
}

const NEW_KEY_TYPES: { value: NewKeyType; label: string }[] = [
  { value: 'String', label: 'String' },
  { value: 'Hash', label: 'Hash' },
  { value: 'List', label: 'List' },
  { value: 'Set', label: 'Set' },
  { value: 'ZSet', label: 'Sorted set' },
];

/** The toolbar's `New key…`: a name, a type and the key's first value. */
export function NewKeyDialog({ onSubmit, onCancel }: { onSubmit(k: NewKey): void; onCancel(): void }) {
  const [k, setK] = useState<NewKey>({ name: '', type: 'String', value: '', field: '', score: 0 });
  const [score, setScore] = useState('0');
  const ids = { name: useId(), type: useId(), field: useId(), value: useId(), score: useId() };
  const scoreOk = k.type !== 'ZSet' || (score.trim() !== '' && Number.isFinite(Number(score)));
  const ok = k.name !== '' && (k.type !== 'Hash' || k.field !== '') && scoreOk;
  const submit = (e: FormEvent) => {
    e.preventDefault();
    if (ok) onSubmit({ ...k, score: Number(score) });
  };
  const member = k.type === 'Set' || k.type === 'ZSet';
  return (
    <Modal title="New key" onClose={onCancel} className="size-s" testId="new-key">
      <form onSubmit={submit}>
        <div className="modal-body flow">
          <div className="field">
            <label htmlFor={ids.name}>Name</label>
            <input id={ids.name} value={k.name} onChange={(e) => setK({ ...k, name: e.target.value })} />
          </div>
          <div className="field">
            <label htmlFor={ids.type}>Type</label>
            <select id={ids.type} value={k.type} onChange={(e) => setK({ ...k, type: e.target.value as NewKeyType })}>
              {NEW_KEY_TYPES.map((t) => (
                <option key={t.value} value={t.value}>
                  {t.label}
                </option>
              ))}
            </select>
          </div>
          {k.type === 'Hash' ? (
            <div className="field">
              <label htmlFor={ids.field}>Field</label>
              <input id={ids.field} value={k.field} onChange={(e) => setK({ ...k, field: e.target.value })} />
            </div>
          ) : null}
          <div className="field">
            <label htmlFor={ids.value}>{member ? 'Member' : 'Value'}</label>
            <input id={ids.value} value={k.value} onChange={(e) => setK({ ...k, value: e.target.value })} />
          </div>
          {k.type === 'ZSet' ? (
            <div className="field">
              <label htmlFor={ids.score}>Score</label>
              <input id={ids.score} inputMode="decimal" value={score} aria-invalid={scoreOk ? undefined : true} onChange={(e) => setScore(e.target.value)} />
            </div>
          ) : null}
        </div>
        <footer className="modal-foot">
          <button type="button" className="btn" onClick={onCancel}>
            Cancel
          </button>
          <button type="submit" className="btn primary" disabled={!ok}>
            Create
          </button>
        </footer>
      </form>
    </Modal>
  );
}
