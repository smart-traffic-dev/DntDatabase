import type { PendingApply } from '../../store/dataEdits';
import { Modal } from '../../dialogs/Modal';
import { Callout, FooterNote, SqlBlock } from '../../dialogs/parts';
import { TableIcon } from '../../components/icons';
import { plural } from '../../lib/plural';
import './dataEdit.css';

interface Props {
  pending: PendingApply;
  onConfirm(): void;
  onCancel(): void;
}

const KINDS = ['UPDATE', 'INSERT', 'DELETE'] as const;

/** How many statements of each kind (by their first keyword). */
function countKinds(statements: string[]): Record<(typeof KINDS)[number], number> {
  const out = { UPDATE: 0, INSERT: 0, DELETE: 0 };
  for (const s of statements) {
    const head = s.trimStart().slice(0, 6).toUpperCase();
    if (head in out) out[head as keyof typeof out]++;
  }
  return out;
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
  const counts = countKinds(preview.statements);
  // Every batch starts on Cancel, so Enter never confirms it by accident.
  const cancel = () => !busy && onCancel();
  return (
    <Modal
      title="Review changes"
      subtitle={blocked ? "These changes can't be saved" : `${plural(preview.statements.length, 'statement')} · nothing runs until you confirm`}
      icon={<TableIcon />}
      tone={blocked ? 'danger' : counts.DELETE > 0 ? 'warning' : 'accent'}
      onClose={cancel}
      closable={!busy}
      className="size-m"
      testId="edit-preview"
      initialFocus="[data-cancel]"
    >
      <div className="modal-body flow">
        {notice ? <p role="status">{notice}</p> : null}
        <div className="ep-summary" aria-label="Summary">
          {KINDS.filter((k) => counts[k] > 0).map((k) => (
            <span key={k} className={`ep-chip ep-chip-${k.toLowerCase()}`}>
              {counts[k]} {k.toLowerCase()}{counts[k] === 1 ? '' : 's'}
            </span>
          ))}
        </div>
        {blocked && preview.reason ? <Callout tone="danger" role="alert">{preview.reason}</Callout> : null}
        {preview.verdict === 'confirm' && !blocked ? (
          <Callout tone="warning" role="note">Confirmation required: check the statements before they run.</Callout>
        ) : null}
        {overwrite ? <Callout tone="warning">Overwrite: the rows are changed even if they changed on the server since they were loaded.</Callout> : null}
        {preview.atomic === 'yes' ? null : (
          <Callout tone="neutral" role="note">Changes cannot be undone: statements that ran before an error stay applied.</Callout>
        )}
        {unchecked.length > 0 ? <p className="hint">Unchecked for conflicts: {unchecked.join(', ')}</p> : null}
        {keyOnly ? <p className="hint">Unchecked for conflicts: every column except the key.</p> : null}
        <SqlBlock sql={preview.statements.map((s) => `${s};`).join('\n')} caption={plural(preview.statements.length, 'statement')} />
        {error ? (
          <Callout tone="danger" role="alert">
            {error}
          </Callout>
        ) : null}
      </div>
      <footer className="modal-foot">
        <FooterNote>{busy ? 'Saving…' : null}</FooterNote>
        <button type="button" className="btn" data-cancel disabled={busy} onClick={cancel}>
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
