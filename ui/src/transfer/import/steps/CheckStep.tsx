import { useEffect, useState } from 'react';
import { api, toAppError } from '../../../api';
import type { JobLogLine } from '../../../bindings';
import { progressText, stateLabel } from '../../../jobs/jobFormat';
import { isLive, useJobs } from '../../../store/jobs';
import { buildRequest } from '../importState';
import type { StepProps } from './stepProps';

/** Lines of the dry run's log shown as "first errors". */
const FIRST_ERRORS = 20;

/** Step 5 (optional): the dry run (R18) as a job, its live progress and its first errors. */
export function CheckStep({ state, dispatch }: StepProps) {
  const target = state.target;
  const run = state.dryRun;
  const job = useJobs((s) => (run ? s.jobs[run.jobId] : undefined));
  const [starting, setStarting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [confirmed, setConfirmed] = useState(false);
  const [problems, setProblems] = useState<{ jobId: string; lines: JobLogLine[] } | null>(null);
  const ended = !!job && !isLive(job.state);
  const production = target?.env === 'production';
  const readOnly = !!target?.read_only;

  // A finished dry run: its first errors and warnings from the job log.
  useEffect(() => {
    if (!run || !ended) return;
    let live = true;
    useJobs
      .getState()
      .detail(run.jobId)
      .then(
        (d) => live && setProblems({ jobId: run.jobId, lines: d.log.filter((l) => l.level !== 'info').slice(0, FIRST_ERRORS) }),
        () => undefined,
      );
    return () => {
      live = false;
    };
  }, [run, ended]);

  const start = async (server: boolean) => {
    const request = buildRequest(state);
    if (!request) return;
    setError(null);
    setStarting(true);
    try {
      const jobId = await api.importDryRun({ request, server_check: server, confirmed: server && production && confirmed });
      dispatch({ type: 'dryRun', jobId, server });
      void useJobs.getState().load();
    } catch (e) {
      setError(toAppError(e).message);
    } finally {
      setStarting(false);
    }
  };

  const busy = starting || (!!job && isLive(job.state)) || (!!run && !job);
  const lines = problems && run && problems.jobId === run.jobId ? problems.lines : [];

  return (
    <div className="stack">
      <p className="hint">
        Recommended before importing. <strong>Validate file</strong> reads the whole file and converts every value.{' '}
        <strong>Validate on server</strong> also inserts the rows into a temporary copy of the table, which is dropped at the end;
        the table itself is not changed.
      </p>
      <p className="hint">Not checked: foreign keys, uniqueness against the rows already in the table, and triggers.</p>
      {production && !readOnly && (
        <label className="check import-wizard-check">
          <input type="checkbox" checked={confirmed} onChange={(e) => setConfirmed(e.target.checked)} />
          <span>Allow the server check on this Production connection</span>
        </label>
      )}
      <div className="row import-wizard-actions">
        <button type="button" className="btn" disabled={busy} onClick={() => void start(false)}>
          Validate file
        </button>
        <button type="button" className="btn" disabled={busy || readOnly || (production && !confirmed)} onClick={() => void start(true)}>
          Validate on server (temporary table)
        </button>
      </div>
      {readOnly && <p className="hint">Read-only connection: the server check is not available.</p>}
      {error && (
        <p role="alert" className="field-error">
          {error}
        </p>
      )}
      {run && (
        <section className="import-wizard-run" data-testid="import-dry-run" aria-label="Validation">
          <p>
            <strong>{run.server ? 'Server check' : 'File check'}</strong>
            {' · '}
            {job ? stateLabel(job) : 'Queued'}
            {job && isLive(job.state) && progressText(job.progress) !== '' ? ` · ${progressText(job.progress)}` : ''}
          </p>
          {job?.summary && <p>{job.summary}</p>}
          {job?.error && (
            <p role="alert" className="field-error">
              {job.error.message}
            </p>
          )}
          {lines.length > 0 && (
            <ul className="import-wizard-log">
              {lines.map((l, i) => (
                <li key={i} className={l.level}>
                  {l.message}
                </li>
              ))}
            </ul>
          )}
        </section>
      )}
    </div>
  );
}
