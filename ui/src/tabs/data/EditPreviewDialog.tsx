import type { PendingApply } from '../../store/dataEdits';
import { Modal } from '../../dialogs/Modal';
import './dataEdit.css';

interface Props {
  pending: PendingApply;
  onConfirm(): void;
  onCancel(): void;
}

/** Spec §4.2: the exact statements of the staged changes, their verdict and conflict checks; nothing runs until Confirm. */
export function EditPreviewDialog({ pending, onConfirm, onCancel }: Props) {
  const { preview, notice, busy, error, overwrite } = pending;
  const blocked = preview.verdict === 'block';
  // The columns a statement's guard cannot compare, once each.
  const unchecked = [
    ...new Set(preview.protections.flatMap((p) => (p.level === 'columns' ? p.unchecked : []))),
  ];
  const keyOnly = preview.protections.some((p) => p.level === 'key_only');
  return (
    <Modal title="Review changes" onClose={onCancel} className="wide" testId="edit-preview" initialFocus="[data-confirm], [data-cancel]">
      <div className="modal-body stack">
        {notice ? <p role="status">{notice}</p> : null}
        <p>
          Verdict: <strong className={`ep-verdict ep-${preview.verdict}`}>{preview.verdict}</strong>
        </p>
        {blocked && preview.reason ? <p role="alert">{preview.reason}</p> : null}
        {overwrite ? <p>Overwrite: the rows are changed even if they changed on the server since they were loaded.</p> : null}
        {preview.atomic === 'yes' ? null : (
          <p role="note">Changes cannot be undone: statements that ran before an error stay applied.</p>
        )}
        {unchecked.length > 0 ? <p>Unchecked for conflicts: {unchecked.join(', ')}</p> : null}
        {keyOnly ? <p>Unchecked for conflicts: every column except the key.</p> : null}
        <ol className="ep-statements">
          {preview.statements.map((sql, i) => (
            <li key={i}>
              <code>{sql}</code>
            </li>
          ))}
        </ol>
        {error ? (
          <p role="alert" className="field-error">
            {error}
          </p>
        ) : null}
      </div>
      <footer className="modal-foot">
        <button type="button" className="btn" data-cancel onClick={onCancel}>
          Cancel
        </button>
        {blocked ? null : (
          <button type="button" className="btn primary" data-confirm disabled={busy} onClick={onConfirm}>
            Confirm
          </button>
        )}
      </footer>
    </Modal>
  );
}
