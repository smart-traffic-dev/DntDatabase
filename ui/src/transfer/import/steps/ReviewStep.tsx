import type { DataImportPreview, ObjectRef } from '../../../bindings';
import { formatBytes } from '../../../jobs/jobFormat';

/** What the user confirms with (R20): typed name, "I understand", irreversible acknowledgement. */
export interface Confirmation {
  typed: string;
  understood: boolean;
  irreversibleAck: boolean;
}

/** The confirmation `preview` asks for is complete. */
export function confirmationDone(preview: DataImportPreview, c: Confirmation): boolean {
  if (preview.pre_step?.irreversible && !c.irreversibleAck) return false;
  switch (preview.confirm.kind) {
    case 'none':
      return true;
    case 'click':
      return c.understood;
    case 'type_name':
      return c.typed === preview.confirm.name;
  }
}

const qualified = (t: ObjectRef) => [t.database, t.schema, t.name].filter(Boolean).join('.');

interface Props {
  preview: DataImportPreview | null;
  preparing: boolean;
  /** Preparing or starting failed; the plan (if any) is used up. */
  error: string | null;
  onPrepareAgain(): void;
  confirmation: Confirmation;
  onConfirmation(c: Confirmation): void;
  readOnly: boolean;
}

/** Step 6: the plan `import_prepare` built, and the confirmation per R20. */
export function ReviewStep({ preview, preparing, error, onPrepareAgain, confirmation: c, onConfirmation, readOnly }: Props) {
  return (
    <div className="stack" data-testid="import-review">
      {preparing && (
        <p role="status" className="hint">
          Preparing the import…
        </p>
      )}
      {error && (
        <div className="stack">
          <p role="alert" className="field-error">
            {error}
          </p>
          <div className="row">
            <button type="button" className="btn" disabled={preparing} onClick={onPrepareAgain}>
              Prepare again
            </button>
          </div>
        </div>
      )}
      {readOnly && <p className="import-wizard-note">This connection is read-only: importing is blocked.</p>}
      {preview && (
        <>
          <dl className="kv">
            <dt>File</dt>
            <dd>
              {preview.file.name} · {formatBytes(preview.file.size)} · SHA-256 <span className="mono">{preview.file.sha256.slice(0, 12)}</span>
            </dd>
            <dt>Target</dt>
            <dd className="mono">{qualified(preview.table)}</dd>
            <dt>Transaction</dt>
            <dd>
              {preview.transactional
                ? 'One transaction: nothing is kept if the import stops.'
                : 'Not transactional: batches already written stay if the import stops.'}
            </dd>
          </dl>

          {preview.pre_step && (
            <section className="stack" aria-label="Before the inserts">
              <h3 className="import-heading">Before the inserts</h3>
              <pre className="import-wizard-sql">{preview.pre_step.sql}</pre>
              {preview.pre_step.warning && <p className="import-wizard-note">{preview.pre_step.warning}</p>}
              {preview.pre_step.irreversible && (
                <label className="check import-wizard-check">
                  <input type="checkbox" checked={c.irreversibleAck} onChange={(e) => onConfirmation({ ...c, irreversibleAck: e.target.checked })} />
                  <span>I understand that existing rows are deleted even if the import fails later</span>
                </label>
              )}
            </section>
          )}

          <section className="stack" aria-label="Insert statements">
            <h3 className="import-heading">INSERT template</h3>
            <pre className="import-wizard-sql">
              {preview.insert_head}
              {'(…), (…)'}
              {preview.insert_tail}
            </pre>
            {preview.sample && (
              <>
                <h3 className="import-heading">First rows</h3>
                <pre className="import-wizard-sql">{preview.sample}</pre>
              </>
            )}
          </section>

          {preview.warnings.length > 0 && (
            <ul className="import-issues warn">
              {preview.warnings.map((w) => (
                <li key={w}>{w}</li>
              ))}
            </ul>
          )}

          {preview.confirm.kind === 'click' && (
            <label className="check import-wizard-check">
              <input type="checkbox" checked={c.understood} onChange={(e) => onConfirmation({ ...c, understood: e.target.checked })} />
              <span>I understand</span>
            </label>
          )}
          {preview.confirm.kind === 'type_name' && (
            <label className="field">
              <span className="field-label">
                Type <code className="run-confirm-name">{preview.confirm.name}</code> to confirm
              </span>
              <input
                type="text"
                className="run-confirm-input"
                autoComplete="off"
                autoCorrect="off"
                spellCheck={false}
                value={c.typed}
                onChange={(e) => onConfirmation({ ...c, typed: e.target.value })}
              />
            </label>
          )}
          <p className="hint">This review is valid for {Math.max(1, Math.round(preview.expires_in_seconds / 60))} min.</p>
        </>
      )}
    </div>
  );
}
