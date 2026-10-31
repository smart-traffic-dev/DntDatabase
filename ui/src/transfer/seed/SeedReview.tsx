import type { ObjectRef, SeedPreview } from '../../bindings';
import { formatCount } from '../../lib/plural';
import type { Confirmation } from '../import/steps/ReviewStep';

const qualified = (t: ObjectRef) => [t.database, t.schema, t.name].filter(Boolean).join('.');

interface Props {
  preview: SeedPreview;
  confirmation: Confirmation;
  onConfirmation(c: Confirmation): void;
}

/** The plan `seed_prepare` built, and the confirmation it asks for (spec §6.2 Review). */
export function SeedReview({ preview, confirmation: c, onConfirmation }: Props) {
  const batches = `${formatCount(preview.batches)} ${preview.batches === 1 ? 'batch' : 'batches'}`;
  const how = preview.transactional ? `in 1 transaction (${batches})` : `without a transaction (${batches})`;
  return (
    <div className="stack" data-testid="seed-review">
      <p className="seed-summary">
        Insert {formatCount(preview.rows)} rows into <span className="mono">{qualified(preview.table)}</span> {how}.
      </p>

      {preview.pre_step && (
        <section className="stack" aria-label="Before the inserts">
          <h3 className="seed-heading">
            Before the inserts
            {preview.pre_step.irreversible && <span className="seed-danger-chip">Cannot be undone</span>}
          </h3>
          <pre className="seed-sql">{preview.pre_step.sql}</pre>
        </section>
      )}

      <section className="stack" aria-label="Insert statements">
        <h3 className="seed-heading">INSERT template</h3>
        {/* One string: the head ends and the tail starts with their own spaces (verbatim). */}
        <pre className="seed-sql">{`${preview.insert_head}(…), (…), …${preview.insert_tail}`}</pre>
        {preview.sample && (
          <>
            <h3 className="seed-heading">First rows</h3>
            <pre className="seed-sql">{preview.sample}</pre>
          </>
        )}
      </section>

      {preview.warnings.length > 0 && (
        <ul className="seed-warnings">
          {preview.warnings.map((w, i) => (
            <li key={i}>{w}</li>
          ))}
        </ul>
      )}

      {preview.pre_step?.irreversible && (
        <label className="check seed-check">
          <input type="checkbox" checked={c.irreversibleAck} onChange={(e) => onConfirmation({ ...c, irreversibleAck: e.target.checked })} />
          <span>I understand existing rows are deleted even if the job fails.</span>
        </label>
      )}
      {preview.confirm.kind === 'click' && (
        <label className="check seed-check">
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
    </div>
  );
}
