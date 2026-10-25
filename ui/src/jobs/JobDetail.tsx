import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import { api, toAppError } from '../api';
import type { JobDetail as JobDetailData, JobLogLine, JobSummary } from '../bindings';
import { formatElapsed } from '../components/statusFormat';
import { timeText } from '../components/historyModel';
import { plural } from '../lib/plural';
import { useNow } from '../lib/useNow';
import { useConnections } from '../store/connections';
import { isLive, POLL_MS, useJobs } from '../store/jobs';
import { KIND_LABEL, elapsedMs, progressPercent, progressText, stateLabel, toneOf } from './jobFormat';
import { canRerun, rerun } from './rerun';

/** A progress bar: determinate when the total is known, else an indeterminate stripe. */
export function JobProgressBar({ job }: { job: JobSummary }) {
  const percent = progressPercent(job.progress);
  return (
    <div
      className={`jb-bar${percent === null ? ' indeterminate' : ''}${job.state === 'queued' ? ' queued' : ''}`}
      role="progressbar"
      aria-label={`Progress of ${job.title}`}
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={percent ?? undefined}
    >
      <span className="jb-bar-fill" style={percent === null ? undefined : { width: `${percent}%` }} />
    </div>
  );
}

/** The state chip: Queued / Running / Done / Failed / Cancelled / Outcome unknown. */
export function JobStateChip({ job }: { job: JobSummary }) {
  return (
    <span className={`chip jb-state ${toneOf(job)}`} data-testid="job-state">
      {stateLabel(job)}
    </span>
  );
}

const LEVEL: Record<JobLogLine['level'], string> = { info: 'INFO', warn: 'WARN', error: 'ERROR' };

const lineText = (l: JobLogLine) => `${timeText(l.at)}  ${LEVEL[l.level].padEnd(5)}  ${l.message}`;

/**
 * One job of the Jobs panel: what it is, its state and the log (newest at the bottom, with the
 * number of lines the backend dropped from the middle), plus Stop, Open folder, Run again and Copy log.
 */
export function JobDetail({ id, onBack }: { id: string; onBack(): void }) {
  const job = useJobs((s) => s.jobs[id]);
  const connection = useConnections((s) => (job?.connection_id ? s.summaries.get(job.connection_id)?.config.name : undefined));
  const [detail, setDetail] = useState<JobDetailData | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const logRef = useRef<HTMLPreElement>(null);
  const backRef = useRef<HTMLButtonElement>(null);
  const live = job ? isLive(job.state) : false;
  const now = useNow(live, null);
  const state = job?.state;

  // The log is read again when the state changes, and every poll interval while the job runs.
  useEffect(() => {
    let alive = true;
    const read = () =>
      useJobs
        .getState()
        .detail(id)
        .then((d) => {
          if (!alive) return;
          setDetail(d);
          setError(null);
        })
        .catch((e: unknown) => alive && setError(toAppError(e).message));
    void read();
    const timer = live ? setInterval(() => void read(), POLL_MS) : null;
    return () => {
      alive = false;
      if (timer) clearInterval(timer);
    };
  }, [id, state, live]);

  // Opening the detail moves the keyboard focus to Back (the row it came from is gone).
  const shown = !!job;
  useEffect(() => {
    if (shown) backRef.current?.focus();
  }, [shown]);

  // Newest at the bottom: keep the end in view as lines arrive.
  useLayoutEffect(() => {
    const el = logRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [detail?.log.length]);

  if (!job) return null;

  const elapsed = elapsedMs(job, now);
  const log = detail?.log ?? [];
  const params = detail?.params ?? null;
  const showRerun = job.rerunnable && canRerun(params);

  const act = async (fn: () => Promise<unknown>) => {
    setError(null);
    try {
      await fn();
    } catch (e) {
      setError(toAppError(e).message);
    }
  };

  const copyLog = () => {
    const text = log.map(lineText).join('\n');
    void navigator.clipboard
      ?.writeText(text)
      .then(() => setNotice('Log copied.'))
      .catch(() => setNotice('Could not copy the log.'));
  };

  return (
    <div className="jb-detail" data-testid="job-detail">
      <div className="jb-detail-head">
        <button ref={backRef} type="button" className="btn small" aria-label="Back to jobs" title="Back to jobs" onClick={onBack}>
          Back
        </button>
        <strong className="jb-detail-title" title={job.title}>
          {job.title}
        </strong>
      </div>
      <dl className="jb-facts">
        <dt>Kind</dt>
        <dd>{KIND_LABEL[job.kind]}</dd>
        {connection && (
          <>
            <dt>Connection</dt>
            <dd>{connection}</dd>
          </>
        )}
        <dt>State</dt>
        <dd>
          <JobStateChip job={job} />
        </dd>
        {elapsed !== null && (
          <>
            <dt>Elapsed</dt>
            <dd>{formatElapsed(live ? Math.floor(elapsed / 1000) * 1000 : elapsed)}</dd>
          </>
        )}
        {live && progressText(job.progress) && (
          <>
            <dt>Progress</dt>
            <dd>{progressText(job.progress)}</dd>
          </>
        )}
        {job.summary && (
          <>
            <dt>Summary</dt>
            <dd>{job.summary}</dd>
          </>
        )}
        {job.error && (
          <>
            <dt>Error</dt>
            <dd className="jb-error">{job.error.message}</dd>
          </>
        )}
        {job.outcome_unknown && job.state !== 'done' && (
          <>
            <dt>Outcome</dt>
            <dd className="jb-warn">Unknown: check the target before running it again.</dd>
          </>
        )}
      </dl>
      {live && <JobProgressBar job={job} />}
      <div className="jb-actions">
        {live && (
          <button type="button" className="btn small" onClick={() => void act(() => useJobs.getState().cancel(id))}>
            Stop
          </button>
        )}
        {job.result_path && job.state === 'done' && (
          <button
            type="button"
            className="btn small"
            title={job.result_path}
            onClick={() => void act(() => api.jobRevealResult(id))}
          >
            Open folder
          </button>
        )}
        {showRerun && params && (
          <button type="button" className="btn small" title="Opens the dialog with the same settings" onClick={() => rerun(params)}>
            Run again
          </button>
        )}
        <button type="button" className="btn small" disabled={log.length === 0} onClick={copyLog}>
          Copy log
        </button>
      </div>
      {error && (
        <div className="qr-msg error" role="alert">
          {error}
        </div>
      )}
      {notice && (
        <div className="qr-msg" role="status">
          {notice}
        </div>
      )}
      {detail && detail.dropped_lines > 0 && <div className="jb-dropped">{plural(detail.dropped_lines, 'line')} not shown</div>}
      <pre ref={logRef} className="jb-log" data-testid="job-log" aria-label="Job log" tabIndex={0}>
        {log.length === 0 ? (
          <span className="jb-log-empty">{detail ? 'No log lines.' : 'Loading…'}</span>
        ) : (
          log.map((l, i) => (
            <span key={i} className={`jb-line ${l.level}`}>
              {lineText(l)}
              {'\n'}
            </span>
          ))
        )}
      </pre>
    </div>
  );
}
