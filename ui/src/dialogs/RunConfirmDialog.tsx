import type { PlanStmtView, PlanView, StmtKindLabel, UnboundedWrite } from '../bindings';
import { EnvTag } from '../components/EnvTag';
import { plural } from '../lib/plural';
import { Modal } from './Modal';

interface Props {
  plan: PlanView;
  /** `true` for "Run all N" / "Run anyway", `false` for Cancel / Esc. */
  onDone(run: boolean): void;
}

const LABEL: Record<StmtKindLabel, string> = {
  read: 'READ',
  write: 'WRITE',
  ddl: 'DDL',
  transaction: 'TX',
  other: 'OTHER',
};

/** The sentence for one unbounded write, without the table (shown after it). */
function warningText(w: UnboundedWrite): string {
  const verb = w.op === 'update' ? 'change' : 'delete';
  switch (w.reason) {
    case 'truncate':
      return 'This TRUNCATE will delete every row in';
    case 'always_true':
      return `This ${w.op.toUpperCase()} has a WHERE clause that is always true and will ${verb} every row in`;
    default:
      return `This ${w.op.toUpperCase()} has no WHERE clause and will ${verb} every row in`;
  }
}

function UnboundedWarning({ stmts }: { stmts: PlanStmtView[] }) {
  return (
    <section className="run-confirm-warning" data-testid="run-confirm-unbounded" aria-label="Statements that change every row">
      <p className="run-confirm-warning-title">
        {stmts.length === 1 ? 'This statement changes' : `${stmts.length} statements change`} every row of a table:
      </p>
      <ul className="run-confirm-list">
        {stmts.map((s) => (
          <li key={s.index} data-testid="run-confirm-unbounded-row" className="run-confirm-unbounded-row">
            {(s.unbounded ?? []).map((w, i) => (
              <span key={i} className="run-confirm-warning-text">
                {warningText(w)} {w.table ? <code>{w.table}</code> : 'the table'}.
              </span>
            ))}
            <span className="run-confirm-row">
              <span className="run-confirm-no">#{s.index + 1}</span>
              <code className="run-confirm-preview">{s.preview}</code>
            </span>
          </li>
        ))}
      </ul>
    </section>
  );
}

/**
 * Spec §5.2: the statements of a run that need a confirmation. On Production every
 * Confirm statement is listed; any statement that changes every row of a table
 * (UPDATE/DELETE without WHERE, TRUNCATE; any connection) also gets a warning. One
 * dialog covers both. Cancel is the default.
 */
export function RunConfirmDialog({ plan, onDone }: Props) {
  const total = plan.stmts.length;
  const confirm = plan.stmts.filter((s) => s.verdict === 'confirm');
  const unbounded = confirm.filter((s) => (s.unbounded ?? []).length > 0);
  const production = plan.env_tag === 'production';
  // Off Production every Confirm is normally an unbounded write; list any other one too.
  const listed = production ? confirm : confirm.filter((s) => !unbounded.includes(s));
  const count = plural(total, 'statement');
  const need = confirm.length;
  const runLabel = unbounded.length > 0 ? 'Run anyway' : total === 1 ? 'Run 1 statement' : `Run all ${total}`;
  return (
    <Modal
      title={production ? 'Confirm run on Production' : 'Confirm run'}
      badge={production ? <EnvTag env="production" variant="chip" testId="env-chip" /> : undefined}
      onClose={() => onDone(false)}
      className={production ? 'wide env-production' : 'wide'}
      role="alertdialog"
      initialFocus="[data-cancel]"
      testId="run-confirm"
    >
      <div className="modal-body stack">
        <p data-testid="run-confirm-summary">
          {count} will run on <strong>{plan.connection_name}</strong>
          {production ? (
            <>
              {' '}
              (PRODUCTION). {need} {need === 1 ? 'needs' : 'need'} confirmation:
            </>
          ) : (
            '.'
          )}
        </p>
        {listed.length > 0 && (
          <ul className="run-confirm-list">
            {listed.map((s) => (
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
        )}
        {unbounded.length > 0 && <UnboundedWarning stmts={unbounded} />}
      </div>
      <footer className="modal-foot">
        <button type="button" className="btn" data-cancel data-testid="run-confirm-cancel" onClick={() => onDone(false)}>
          Cancel
        </button>
        <button type="button" className="btn danger" data-testid="run-confirm-run" onClick={() => onDone(true)}>
          {runLabel}
        </button>
      </footer>
    </Modal>
  );
}
