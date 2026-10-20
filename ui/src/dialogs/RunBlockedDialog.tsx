import type { PlanView } from '../bindings';
import { Modal } from './Modal';

interface Props {
  plan: PlanView;
  onClose(): void;
}

/** Spec §5.2: statements a read-only connection refuses. Nothing runs; the only button is Close. */
export function RunBlockedDialog({ plan, onClose }: Props) {
  const blocked = plan.stmts.filter((s) => s.verdict === 'block');
  return (
    <Modal
      title="Blocked on read-only connection"
      onClose={onClose}
      className="wide"
      role="alertdialog"
      initialFocus="[data-close]"
      testId="run-blocked"
    >
      <div className="modal-body stack">
        <p>{plan.connection_name} is read-only. These statements were not run:</p>
        <ul className="run-confirm-list">
          {blocked.map((s) => (
            <li key={s.index} className="run-confirm-row">
              <span className="run-confirm-no">#{s.index + 1}</span>
              <code className="run-confirm-preview">{s.preview}</code>
            </li>
          ))}
        </ul>
      </div>
      <footer className="modal-foot">
        <button type="button" className="btn primary" data-close onClick={onClose}>
          Close
        </button>
      </footer>
    </Modal>
  );
}
