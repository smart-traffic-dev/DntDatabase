import { useEffect, useMemo, useReducer, useRef, useState } from 'react';
import { api, toAppError } from '../../api';
import type { AppError, ObjectRef, SeedRequest } from '../../bindings';
import { EnvTag } from '../../components/EnvTag';
import { showJobs } from '../../components/windowActions';
import { Modal } from '../../dialogs/Modal';
import { useJobs } from '../../store/jobs';
import { useToasts } from '../../store/toasts';
import { cellView } from '../../tabs/query/format';
import { confirmationDone, type Confirmation } from '../import/steps/ReviewStep';
import { ColumnsTable } from './ColumnsTable';
import { SeedReview } from './SeedReview';
import { buildRequest, initialState, MAX_ROWS, reduce } from './seedState';
import './seed.css';

interface Props {
  connectionId: string;
  table: ObjectRef;
  /** Pre-filled from "Run again". */
  params?: SeedRequest;
  onClose(): void;
}

/** How long typing settles before the preview is asked again. */
export const PREVIEW_DEBOUNCE_MS = 300;

const NO_CONFIRMATION: Confirmation = { typed: '', understood: false, irreversibleAck: false };

const qualified = (t: ObjectRef) => [t.database, t.schema, t.name].filter(Boolean).join('.');

/** The message of a failed prepare or start (the plan is used up either way). */
function reviewMessage(err: AppError): string {
  if (err.kind === 'plan_stale') return 'The table changed; review again.';
  if (err.kind === 'plan_expired') return 'The review expired; review again.';
  return err.message;
}

const randomSeed = () => {
  const a = new Uint32Array(1);
  crypto.getRandomValues(a);
  return a[0] ?? 0;
};

/**
 * The data seeder (spec §6.2): Configure (rows, existing rows, seed, a generator per column, a live
 * preview) then Review (the plan `seed_prepare` built, the confirmation). Generating runs as a job.
 */
export function SeedDialog({ connectionId, table, params, onClose }: Props) {
  const [state, dispatch] = useReducer(reduce, { connectionId, table, params }, initialState);
  const [targetError, setTargetError] = useState<string | null>(null);
  const [previewError, setPreviewError] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [previewing, setPreviewing] = useState(false);
  const [preparing, setPreparing] = useState(false);
  const [starting, setStarting] = useState(false);
  const [refreshes, setRefreshes] = useState(0);
  const [confirmation, setConfirmation] = useState<Confirmation>(NO_CONFIRMATION);
  const { target, sample, preview, step } = state;

  useEffect(() => {
    let live = true;
    api.seedTarget(connectionId, table).then(
      (result) => live && dispatch({ type: 'targetLoaded', target: result }),
      (e) => live && setTargetError(toAppError(e).message),
    );
    return () => {
      live = false;
    };
  }, [connectionId, table]);

  // The live preview: asked again once edits settle; only the newest answer applies.
  const request = buildRequest(state);
  const requestKey = request ? JSON.stringify(request) : null;
  const seq = useRef(0);
  useEffect(() => {
    if (!requestKey) return;
    const my = ++seq.current;
    setPreviewing(true);
    const timer = setTimeout(() => {
      api.seedPreview(JSON.parse(requestKey) as SeedRequest).then(
        (result) => {
          if (my !== seq.current) return;
          setPreviewError(null);
          dispatch({ type: 'sampled', sample: result });
          setPreviewing(false);
        },
        (e) => {
          if (my !== seq.current) return;
          setPreviewError(toAppError(e).message);
          setPreviewing(false);
        },
      );
    }, PREVIEW_DEBOUNCE_MS);
    return () => clearTimeout(timer);
  }, [requestKey, refreshes]);

  // A changed request makes an old review error moot.
  useEffect(() => setError(null), [requestKey]);

  const keyCounts = useMemo(() => new Map((sample?.fk_keys ?? []).map((k) => [k.constraint, k.count])), [sample]);
  const readOnly = !!target?.read_only;
  const production = target?.env === 'production';
  const issues = sample?.issues ?? [];
  const canReview = !!request && !!sample && !readOnly && issues.length === 0 && !previewing && !preparing && !previewError;
  const canStart = !!preview && !readOnly && !starting && confirmationDone(preview, confirmation);

  const prepare = async () => {
    if (!request) return;
    setPreparing(true);
    setError(null);
    try {
      const result = await api.seedPrepare(request);
      setConfirmation(NO_CONFIRMATION);
      dispatch({ type: 'prepared', preview: result });
    } catch (e) {
      setError(reviewMessage(toAppError(e)));
    } finally {
      setPreparing(false);
    }
  };

  const start = async () => {
    if (!preview) return;
    setStarting(true);
    try {
      const jobId = await api.seedStart(preview.plan_id, {
        confirmed: true,
        name: preview.confirm.kind === 'type_name' ? confirmation.typed : null,
        irreversible_ack: confirmation.irreversibleAck,
      });
      setStarting(false);
      onClose();
      useToasts.getState().show({ kind: 'info', title: 'Generating data… — see Jobs' });
      useJobs.getState().openJob(jobId);
      showJobs();
      void useJobs.getState().load();
    } catch (e) {
      // The plan is single use: whatever failed, the review must be prepared again.
      dispatch({ type: 'back' });
      setError(reviewMessage(toAppError(e)));
      setStarting(false);
    }
  };

  const close = () => {
    if (!starting) onClose();
  };

  return (
    <Modal
      title={`Generate data — ${qualified(table)}`}
      badge={production ? <EnvTag env="production" variant="chip" /> : undefined}
      onClose={close}
      className={`seed-dialog${production ? ' env-production' : ''}`}
      testId="seed-dialog"
    >
      <div className="seed-body modal-body">
        {targetError && (
          <p role="alert" className="field-error">
            {targetError}
          </p>
        )}
        {readOnly && <p className="seed-note">This connection is read-only: generating data is blocked.</p>}
        {error && (
          <p role="alert" className="field-error">
            {error}
          </p>
        )}
        {step === 'configure' && (
          <>
            <div className="seed-options">
              <label className="field">
                <span className="field-label">Rows</span>
                <input
                  type="number"
                  min={1}
                  max={MAX_ROWS}
                  value={state.rows}
                  onChange={(e) => Number.isFinite(e.target.valueAsNumber) && dispatch({ type: 'setRows', rows: Math.trunc(e.target.valueAsNumber) })}
                />
              </label>
              <label className="field">
                <span className="field-label">Batch size</span>
                <input
                  type="number"
                  min={1}
                  value={state.batchRows}
                  onChange={(e) => Number.isFinite(e.target.valueAsNumber) && dispatch({ type: 'setBatch', batchRows: Math.trunc(e.target.valueAsNumber) })}
                />
              </label>
              <label className="field">
                <span className="field-label">Existing rows</span>
                <select value={state.existing} onChange={(e) => dispatch({ type: 'setExisting', existing: e.target.value as SeedRequest['existing'] })}>
                  <option value="keep">Keep</option>
                  <option value="delete">Delete first</option>
                  <option value="truncate">Truncate first</option>
                </select>
              </label>
              <label className="field">
                <span className="field-label">Seed</span>
                <span className="seed-seed">
                  <input
                    type="number"
                    min={0}
                    value={state.seed}
                    onChange={(e) =>
                      Number.isFinite(e.target.valueAsNumber) && dispatch({ type: 'setSeed', seed: Math.min(0xffffffff, Math.max(0, Math.trunc(e.target.valueAsNumber))) })
                    }
                  />
                  <button type="button" className="btn" title="New seed" aria-label="New seed" onClick={() => dispatch({ type: 'setSeed', seed: randomSeed() })}>
                    🎲
                  </button>
                </span>
              </label>
            </div>

            {target ? (
              <ColumnsTable
                columns={target.columns}
                plans={state.plans}
                issues={issues}
                keyCounts={keyCounts}
                onGenerator={(column, generator) => dispatch({ type: 'setGenerator', column, generator })}
                onPlan={(column, patch) => dispatch({ type: 'setPlan', column, patch })}
              />
            ) : (
              !targetError && (
                <p role="status" className="hint">
                  Reading the table…
                </p>
              )
            )}

            <section className="stack" aria-label="Preview">
              <div className="seed-preview-head">
                <h3 className="seed-heading">Preview — first {Math.min(10, state.rows)} rows</h3>
                <span className="spacer" />
                {previewing && <span className="hint">Updating…</span>}
                <button type="button" className="btn" disabled={!request} onClick={() => setRefreshes((n) => n + 1)}>
                  Refresh
                </button>
              </div>
              {previewError && (
                <p role="alert" className="field-error">
                  {previewError}
                </p>
              )}
              {target && sample && (
                <div className="seed-grid" data-testid="seed-preview">
                  <table className="seed-table">
                    <thead>
                      <tr>
                        {target.columns.map((c) => (
                          <th key={c.column.name} scope="col">
                            {c.column.name}
                          </th>
                        ))}
                      </tr>
                    </thead>
                    <tbody>
                      {sample.rows.map((values, r) => (
                        <tr key={r}>
                          {target.columns.map((c) => {
                            const at = sample.columns.indexOf(c.column.name);
                            if (at < 0) {
                              return (
                                <td key={c.column.name} className="seed-null">
                                  (default)
                                </td>
                              );
                            }
                            const view = cellView(values[at] ?? { t: 'Null' });
                            return (
                              <td key={c.column.name} className={view.kind === 'null' ? 'seed-null' : undefined}>
                                {view.text}
                              </td>
                            );
                          })}
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              )}
            </section>
          </>
        )}
        {step === 'review' && preview && <SeedReview preview={preview} confirmation={confirmation} onConfirmation={setConfirmation} />}
      </div>
      <footer className="modal-foot">
        <button type="button" className="btn" disabled={starting} onClick={close}>
          Cancel
        </button>
        <span className="spacer" />
        {step === 'review' ? (
          <>
            <button type="button" className="btn" disabled={starting} onClick={() => dispatch({ type: 'back' })}>
              Back
            </button>
            <button type="button" className={`btn ${production || preview?.pre_step ? 'danger' : 'primary'}`} disabled={!canStart} onClick={() => void start()}>
              Generate
            </button>
          </>
        ) : (
          <button type="button" className="btn primary" disabled={!canReview} onClick={() => void prepare()}>
            Review…
          </button>
        )}
      </footer>
    </Modal>
  );
}
