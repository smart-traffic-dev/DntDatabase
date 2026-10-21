import { useState } from 'react';
import type { DropTarget, PlanStmtView, PlanView, RowEstimate, StmtKindLabel, UnboundedWrite } from '../bindings';
import { EnvTag } from '../components/EnvTag';
import { plural } from '../lib/plural';
import { confirmNamesOf } from '../store/runPrompt';
import { Modal } from './Modal';

interface Props {
  plan: PlanView;
  /**
   * `true` for "Run all N" / "Run anyway", `false` for Cancel / Esc. A run that asks for typed names
   * passes them (exactly the plan's `confirm_name`s, once each) as the second argument.
   */
  onDone(run: boolean, names?: string[]): void;
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

/** "≈ 12,345 rows" or "Row count unavailable"; nothing when the plan has no estimate. */
function Estimate({ estimate }: { estimate: RowEstimate | null | undefined }) {
  if (!estimate) return null;
  return (
    <span className="run-confirm-note" data-testid="run-confirm-estimate">
      {estimate.kind === 'rows' ? `≈ ${plural(estimate.n, 'row')}` : 'Row count unavailable'}
    </span>
  );
}

/** "Drops database shop", "Drops schema app", "Drops orders" (what a DROP of the statement removes). */
function dropsText(drops: DropTarget[]): string {
  const part = (d: DropTarget) => {
    if (d.kind === 'object') return d.name || 'an object';
    return `${d.kind} ${d.name}`.trim();
  };
  return `Drops ${drops.map(part).join(', ')}`;
}

function UnboundedWarning({ stmts, showEstimate }: { stmts: PlanStmtView[]; showEstimate: boolean }) {
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
            {showEstimate && <Estimate estimate={s.estimate} />}
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

/** The statements that commit the open transaction when they run (MySQL DDL and the like). */
function ImplicitCommitWarning({ stmts }: { stmts: PlanStmtView[] }) {
  return (
    <section className="run-confirm-warning" data-testid="run-confirm-implicit-commit" aria-label="Statements that commit">
      <ul className="run-confirm-list">
        {stmts.map((s) => (
          <li key={s.index} className="run-confirm-unbounded-row">
            <span className="run-confirm-warning-text">This statement commits the open transaction.</span>
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
 * (UPDATE/DELETE without WHERE, TRUNCATE; any connection) also gets a warning, with the server's
 * row estimate when there is one. A statement that asks for a typed name (`confirm_name`) gets a
 * box, and Run stays off until each matches exactly. One dialog covers it all. Cancel is the default:
 * it has the focus; Enter in a name box does nothing (the Modal's alertdialog rule).
 */
export function RunConfirmDialog({ plan, onDone }: Props) {
  const total = plan.stmts.length;
  const confirm = plan.stmts.filter((s) => s.verdict === 'confirm');
  const unbounded = confirm.filter((s) => (s.unbounded ?? []).length > 0);
  const production = plan.env_tag === 'production';
  // Off Production every Confirm is normally an unbounded write; list any other one too.
  const listed = production ? confirm : confirm.filter((s) => !unbounded.includes(s));
  const implicit = plan.stmts.filter((s) => s.implicit_commit === true);
  const names = confirmNamesOf(plan);
  const [typed, setTyped] = useState<Record<string, string>>({});
  const matched = names.every((n) => typed[n] === n);
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
          // Board 08: the statements as one SQL block on the sunken surface.
          <div className="run-confirm-sql">
            <ul className="run-confirm-list">
              {listed.map((s) => (
                <li key={s.index} className="run-confirm-item">
                  <div
                    data-testid="run-confirm-row"
                    className={`run-confirm-row${s.kind === 'ddl' ? ' danger' : ''}`}
                  >
                    <span className="run-confirm-no">#{s.index + 1}</span>
                    <span className="run-confirm-kind">{LABEL[s.kind]}</span>
                    <code className="run-confirm-preview">{s.preview}</code>
                  </div>
                  {(s.drops ?? []).length > 0 && (
                    <span className="run-confirm-note warn" data-testid="run-confirm-drops">
                      {dropsText(s.drops ?? [])}
                    </span>
                  )}
                  <Estimate estimate={s.estimate} />
                </li>
              ))}
            </ul>
          </div>
        )}
        {unbounded.length > 0 && <UnboundedWarning stmts={unbounded} showEstimate={!production} />}
        {implicit.length > 0 && <ImplicitCommitWarning stmts={implicit} />}
        {names.length > 0 && (
          <section className="run-confirm-type" aria-label="Type the names to confirm">
            {names.map((n) => (
              <label key={n} className="field">
                <span className="field-label">
                  Type <code className="run-confirm-name">{n}</code> to confirm
                </span>
                <input
                  type="text"
                  className="run-confirm-input"
                  data-testid="run-confirm-name"
                  autoComplete="off"
                  autoCorrect="off"
                  spellCheck={false}
                  value={typed[n] ?? ''}
                  onChange={(e) => setTyped((t) => ({ ...t, [n]: e.target.value }))}
                />
              </label>
            ))}
          </section>
        )}
      </div>
      <footer className="modal-foot">
        <button type="button" className="btn" data-cancel data-testid="run-confirm-cancel" onClick={() => onDone(false)}>
          Cancel
        </button>
        <button
          type="button"
          className="btn danger"
          data-testid="run-confirm-run"
          disabled={!matched}
          onClick={() => (names.length > 0 ? onDone(true, names) : onDone(true))}
        >
          {runLabel}
        </button>
      </footer>
    </Modal>
  );
}
