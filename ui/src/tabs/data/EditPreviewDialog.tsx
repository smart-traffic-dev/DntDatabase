import type { EngineFamily } from '../../bindings';
import type { PendingApply } from '../../store/dataEdits';
import { Modal } from '../../dialogs/Modal';
import { Callout, FooterNote, SqlBlock } from '../../dialogs/parts';
import { TableIcon } from '../../components/icons';
import { assertNever } from '../../engine';
import { plural } from '../../lib/plural';
import './dataEdit.css';

interface Props {
  pending: PendingApply;
  /** SQL statements are counted by kind; Command-family commands by their total only. */
  family?: EngineFamily;
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

/** What the plan is made of, per family: its unit, the summary chips and the text the statements are shown as. */
function planCopy(family: EngineFamily, pending: PendingApply): { unit: string; chips: { key: string; text: string }[]; text: string; deletes: boolean } {
  const { statements } = pending.preview;
  switch (family) {
    case 'sql': {
      const counts = countKinds(statements);
      return {
        unit: 'statement',
        chips: KINDS.filter((k) => counts[k] > 0).map((k) => ({ key: k.toLowerCase(), text: `${counts[k]} ${k.toLowerCase()}${counts[k] === 1 ? '' : 's'}` })),
        text: statements.map((s) => `${s};`).join('\n'),
        deletes: counts.DELETE > 0,
      };
    }
    case 'command':
      // A command's kind is not in a fixed first word (MongoDB methods, Redis commands): the total only.
      // Whether it deletes comes from the staged changes behind the plan.
      return {
        unit: 'command',
        chips: [{ key: 'command', text: plural(statements.length, 'command') }],
        text: statements.join('\n'),
        deletes: pending.keys.some((k) => k.kind === 'delete'),
      };
    default:
      return assertNever(family);
  }
}

/** Spec §4.2: the exact statements of the staged changes, their verdict and conflict checks; nothing runs until Confirm. */
export function EditPreviewDialog({ pending, family = 'sql', onConfirm, onCancel }: Props) {
  const { preview, notice, busy, error, overwrite } = pending;
  const blocked = preview.verdict === 'block';
  // The columns a statement's guard cannot compare, once each.
  const unchecked = [
    ...new Set(preview.protections.flatMap((p) => (p.level === 'columns' ? p.unchecked : []))),
  ];
  const keyOnly = preview.protections.some((p) => p.level === 'key_only');
  const plan = planCopy(family, pending);
  // Every batch starts on Cancel, so Enter never confirms it by accident.
  const cancel = () => !busy && onCancel();
  return (
    <Modal
      title="Review changes"
      subtitle={blocked ? "These changes can't be saved" : `${plural(preview.statements.length, plan.unit)} · nothing runs until you confirm`}
      icon={<TableIcon />}
      tone={blocked ? 'danger' : plan.deletes ? 'warning' : 'accent'}
      onClose={cancel}
      closable={!busy}
      className="size-m"
      testId="edit-preview"
      initialFocus="[data-cancel]"
    >
      <div className="modal-body flow">
        {notice ? <p role="status">{notice}</p> : null}
        <div className="ep-summary" aria-label="Summary">
          {plan.chips.map((c) => (
            <span key={c.key} className={`ep-chip ep-chip-${c.key}`}>
              {c.text}
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
        <SqlBlock sql={plan.text} caption={plural(preview.statements.length, plan.unit)} />
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
