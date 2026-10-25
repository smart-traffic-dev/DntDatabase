import { useEffect, useReducer, useRef, useState } from 'react';
import { api, toAppError } from '../../api';
import type { AppError, ImportRequest, ObjectRef } from '../../bindings';
import { EnvTag } from '../../components/EnvTag';
import { showJobs } from '../../components/windowActions';
import { Modal } from '../../dialogs/Modal';
import { useJobs } from '../../store/jobs';
import { buildRequest, canAdvance, canOpen, initialState, reduce, STEPS } from './importState';
import { CheckStep } from './steps/CheckStep';
import { MappingStep } from './steps/MappingStep';
import { OptionsStep } from './steps/OptionsStep';
import { confirmationDone, ReviewStep, type Confirmation } from './steps/ReviewStep';
import { SourceStep } from './steps/SourceStep';
import { TargetStep } from './steps/TargetStep';
import './importWizard.css';

interface Props {
  connectionId: string;
  /** Pre-filled from the tree's "Import data…". */
  table?: ObjectRef;
  /** Pre-filled from "Run again" (R5). */
  params?: ImportRequest;
  onClose(): void;
}

const NO_CONFIRMATION: Confirmation = { typed: '', understood: false, irreversibleAck: false };

/** The review's message for a failed prepare or start (the plan is used up either way). */
function reviewMessage(err: AppError): string {
  if (err.kind === 'plan_stale') return 'The file or the table changed; prepare again.';
  if (err.kind === 'plan_expired') return 'The review expired; prepare again.';
  return err.message;
}

/**
 * The data import wizard (spec §6.7): Source · Target · Mapping · Options · Check · Review. The
 * backend reads the file and the table, maps and checks the mapping, runs the dry run as a job and
 * builds the plan; the wizard only collects choices and shows what the backend answered.
 */
export function ImportWizard({ connectionId, table, params, onClose }: Props) {
  const [state, dispatch] = useReducer(reduce, { connectionId, table, params }, initialState);
  const [inspectError, setInspectError] = useState<string | null>(null);
  const [targetError, setTargetError] = useState<string | null>(null);
  const [checkError, setCheckError] = useState<string | null>(null);
  const [reviewError, setReviewError] = useState<string | null>(null);
  const [preparing, setPreparing] = useState(false);
  const [starting, setStarting] = useState(false);
  const [confirmation, setConfirmation] = useState<Confirmation>(NO_CONFIRMATION);

  const { path, source, inspection, target, mapping, mode, overrideIdentity, report, step, preview } = state;

  // The file's preview, read again whenever the file or its options change.
  useEffect(() => {
    if (!path || inspection) return;
    let live = true;
    setInspectError(null);
    api.importInspect(path, source).then(
      (result) => live && dispatch({ type: 'inspected', inspection: result }),
      (e) => live && setInspectError(toAppError(e).message),
    );
    return () => {
      live = false;
    };
  }, [path, source, inspection]);

  // The target's columns and keys, read when the table changes.
  useEffect(() => {
    if (!state.table || target) return;
    let live = true;
    setTargetError(null);
    api.importTarget(state.connectionId, state.table).then(
      (result) => live && dispatch({ type: 'targetLoaded', target: result }),
      (e) => live && setTargetError(toAppError(e).message),
    );
    return () => {
      live = false;
    };
  }, [state.connectionId, state.table, target]);

  // Only the newest auto map applies (the effect's and "Auto map" clicks may overlap); a new file
  // or table also drops an answer still on its way.
  const autoMapSeq = useRef(0);
  const autoMap = () => {
    const my = ++autoMapSeq.current;
    if (!inspection || !target) return;
    api.importAutoMap(inspection.columns, target.columns).then(
      (result) => my === autoMapSeq.current && dispatch({ type: 'mapping', mapping: result }),
      (e) => my === autoMapSeq.current && setCheckError(toAppError(e).message),
    );
  };

  // A fresh file and table are auto-mapped once (stored parameters keep their mapping).
  useEffect(() => {
    if (mapping.length === 0) autoMap();
    const seq = autoMapSeq;
    return () => {
      seq.current++;
    };
    // `autoMap` reads exactly these.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [inspection, target, mapping.length]);

  // A changed request makes a failed review worth preparing again (the old error no longer applies).
  const requestKey = JSON.stringify(buildRequest(state));
  useEffect(() => {
    setReviewError(null);
  }, [requestKey]);

  // The backend checks every mapping, mode and identity choice (the UI never re-implements the rules).
  useEffect(() => {
    if (!target || mapping.length === 0 || report) return;
    let live = true;
    setCheckError(null);
    api.importCheckMapping(mapping, target.columns, target.keys, mode, overrideIdentity).then(
      (result) => live && dispatch({ type: 'report', report: result }),
      (e) => live && setCheckError(toAppError(e).message),
    );
    return () => {
      live = false;
    };
  }, [target, mapping, mode, overrideIdentity, report]);

  // The Review step prepares the plan (again after a failure only when asked).
  useEffect(() => {
    if (step !== 'review' || preview || reviewError) return;
    const request = buildRequest(state);
    if (!request) return;
    let live = true;
    setPreparing(true);
    api.importPrepare(request).then(
      (result) => {
        if (!live) return;
        setConfirmation(NO_CONFIRMATION);
        dispatch({ type: 'prepared', preview: result });
        setPreparing(false);
      },
      (e) => {
        if (!live) return;
        setReviewError(reviewMessage(toAppError(e)));
        setPreparing(false);
      },
    );
    return () => {
      live = false;
      setPreparing(false);
    };
    // The request is built from the state the reducer changes; `preview` is dropped on every change.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [step, preview, reviewError]);

  const start = async () => {
    if (!preview) return;
    setStarting(true);
    setReviewError(null);
    try {
      const jobId = await api.importStart(preview.plan_id, {
        confirmed: true,
        name: preview.confirm.kind === 'type_name' ? confirmation.typed : null,
        irreversible_ack: confirmation.irreversibleAck,
      });
      setStarting(false);
      onClose();
      // The Jobs panel opens on the job just started.
      useJobs.getState().openJob(jobId);
      showJobs();
      void useJobs.getState().load();
    } catch (e) {
      // The plan is single use: whatever failed, the review must be prepared again.
      dispatch({ type: 'unprepare' });
      setReviewError(reviewMessage(toAppError(e)));
      setStarting(false);
    }
  };

  const close = () => {
    if (!starting) onClose();
  };

  const index = STEPS.findIndex((s) => s.id === step);
  const prev = STEPS[index - 1];
  const nextStep = STEPS[index + 1];
  const production = target?.env === 'production';
  const readOnly = !!target?.read_only;
  const canStart = !!preview && !readOnly && !starting && !preparing && confirmationDone(preview, confirmation);

  return (
    <Modal
      title={state.table ? `Import data into ${state.table.name}` : 'Import data'}
      badge={production ? <EnvTag env="production" variant="chip" /> : undefined}
      onClose={close}
      className={`import-wizard${production ? ' env-production' : ''}`}
      testId="import-wizard"
    >
      <div className="import-wizard-body">
        <nav className="import-wizard-nav" aria-label="Import steps">
          {STEPS.map((s, i) => (
            <button
              key={s.id}
              type="button"
              className={`import-wizard-step${s.id === step ? ' on' : ''}`}
              aria-current={s.id === step ? 'step' : undefined}
              disabled={!canOpen(state, s.id)}
              onClick={() => dispatch({ type: 'goto', step: s.id })}
            >
              <span className="import-wizard-step-no">{i + 1}</span>
              {s.label}
            </button>
          ))}
        </nav>
        <div className="import-wizard-main modal-body">
          {step === 'source' && <SourceStep state={state} dispatch={dispatch} inspectError={inspectError} />}
          {step === 'target' && <TargetStep state={state} dispatch={dispatch} targetError={targetError} />}
          {step === 'mapping' && <MappingStep state={state} dispatch={dispatch} onAutoMap={() => void autoMap()} checkError={checkError} />}
          {step === 'options' && <OptionsStep state={state} dispatch={dispatch} />}
          {step === 'check' && <CheckStep state={state} dispatch={dispatch} />}
          {step === 'review' && (
            <ReviewStep
              preview={preview}
              preparing={preparing}
              error={reviewError}
              onPrepareAgain={() => setReviewError(null)}
              confirmation={confirmation}
              onConfirmation={setConfirmation}
              readOnly={readOnly}
            />
          )}
        </div>
      </div>
      <footer className="modal-foot">
        <button type="button" className="btn" disabled={starting} onClick={close}>
          Cancel
        </button>
        <span className="spacer" />
        <button type="button" className="btn" disabled={!prev || starting} onClick={() => prev && dispatch({ type: 'goto', step: prev.id })}>
          Back
        </button>
        {nextStep ? (
          <button type="button" className="btn primary" disabled={!canAdvance(state)} onClick={() => dispatch({ type: 'goto', step: nextStep.id })}>
            Next
          </button>
        ) : (
          <button type="button" className={`btn ${production ? 'danger' : 'primary'}`} disabled={!canStart} onClick={() => void start()}>
            Start import
          </button>
        )}
      </footer>
    </Modal>
  );
}
