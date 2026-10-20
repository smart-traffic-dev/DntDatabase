import type { PlanView, StmtKindLabel } from '../bindings';
import { EnvTag } from '../components/EnvTag';
import { plural } from '../lib/plural';
import { Modal } from './Modal';

interface Props {
  plan: PlanView;
  /** `true` for "Run all N", `false` for Cancel / Esc. */
  onDone(run: boolean): void;
}

const LABEL: Record<StmtKindLabel, string> = {
  read: 'READ',
  write: 'WRITE',
  ddl: 'DDL',
  transaction: 'TX',
  other: 'OTHER',
};

/** Spec §5.2: the statements of a Production run that need a confirmation. Cancel is the default. */
export function RunConfirmDialog({ plan, onDone }: Props) {
  const total = plan.stmts.length;
  const confirm = plan.stmts.filter((s) => s.verdict === 'confirm');
  const count = plural(total, 'statement');
  const need = confirm.length;
  return (
    <Modal
      title="Confirm run on Production"
      badge={<EnvTag env="production" variant="chip" testId="env-chip" />}
      onClose={() => onDone(false)}
      className="wide env-production"
      role="alertdialog"
      initialFocus="[data-cancel]"
      testId="run-confirm"
    >
      <div className="modal-body stack">
        <p data-testid="run-confirm-summary">
          {count} will run on <strong>{plan.connection_name}</strong> (PRODUCTION). {need} {need === 1 ? 'needs' : 'need'}{' '}
          confirmation:
        </p>
        <ul className="run-confirm-list">
          {confirm.map((s) => (
            <li
              key={s.index}
              data-testid="run-confirm-row"
              className={`run-confirm-row${s.kind === 'ddl' ? ' danger' : ''}`}
            >
              <span className="run-confirm-no">#{s.index + 1}</span>
              <span className="run-confirm-kind">{LABEL[s.kind]}</span>
              <code className="run-confirm-preview">{s.preview}</code>
            </li>
          ))}
        </ul>
      </div>
      <footer className="modal-foot">
        <button type="button" className="btn" data-cancel data-testid="run-confirm-cancel" onClick={() => onDone(false)}>
          Cancel
        </button>
        <button type="button" className="btn danger" data-testid="run-confirm-run" onClick={() => onDone(true)}>
          {total === 1 ? 'Run 1 statement' : `Run all ${total}`}
        </button>
      </footer>
    </Modal>
  );
}
