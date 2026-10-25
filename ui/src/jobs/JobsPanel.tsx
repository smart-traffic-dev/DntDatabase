import { useEffect, useState } from 'react';
import { toAppError } from '../api';
import type { JobSummary } from '../bindings';
import { ChevronIcon, TrashIcon } from '../components/icons';
import { formatElapsed } from '../components/statusFormat';
import { timeText } from '../components/historyModel';
import { useNow } from '../lib/useNow';
import { useConnections } from '../store/connections';
import { isLive, useJobs } from '../store/jobs';
import { JobDetail, JobProgressBar, JobStateChip } from './JobDetail';
import { elapsedMs, progressText, stateLabel, toneOf } from './jobFormat';
import './jobs.css';

function JobRow({ job, now, onOpen }: { job: JobSummary; now: number; onOpen(): void }) {
  const connection = useConnections((s) => (job.connection_id ? s.summaries.get(job.connection_id)?.config.name : undefined));
  const [error, setError] = useState<string | null>(null);
  const live = isLive(job.state);
  const elapsed = elapsedMs(job, now);
  const progress = progressText(job.progress);

  const stop = async () => {
    setError(null);
    try {
      await useJobs.getState().cancel(job.id);
    } catch (e) {
      setError(toAppError(e).message);
    }
  };

  return (
    <li className={`jb-item ${toneOf(job)}`} data-testid="job-row" data-job-id={job.id} onClick={onOpen}>
      <div className="jb-row">
        <button type="button" className="jb-open" title={job.title} aria-label={`${job.title}: ${stateLabel(job)}. Show details`}>
          <span className="jb-title">{job.title}</span>
        </button>
        {live ? (
          <button
            type="button"
            className="btn small"
            onClick={(e) => {
              e.stopPropagation();
              void stop();
            }}
          >
            Stop
          </button>
        ) : (
          <JobStateChip job={job} />
        )}
      </div>
      <div className="jb-meta">
        {connection && <span className="jb-conn">{connection}</span>}
        {live ? (
          <>
            <span className="jb-progress-text">{job.state === 'queued' ? 'Waiting for a free slot' : progress || 'Starting'}</span>
            <span className="spacer" />
            {elapsed !== null && <span className="jb-elapsed">{formatElapsed(Math.floor(elapsed / 1000) * 1000)}</span>}
          </>
        ) : (
          <>
            <span className="jb-summary">{job.summary ?? job.error?.message ?? ''}</span>
            <span className="spacer" />
            {job.finished_at && <span className="jb-elapsed">{timeText(job.finished_at)}</span>}
          </>
        )}
      </div>
      {live && <JobProgressBar job={job} />}
      {error && (
        <div className="field-error" role="alert">
          {error}
        </div>
      )}
    </li>
  );
}

/**
 * The Jobs panel of the right sidebar (spec §4.5): live jobs first with their progress, elapsed
 * time and Stop; then finished jobs with their state, summary and end time. A row opens its
 * detail (log, Open folder, Run again, Copy log).
 */
export function JobsPanel({ onCollapse }: { onCollapse?: () => void }) {
  const order = useJobs((s) => s.order);
  const jobs = useJobs((s) => s.jobs);
  const [openId, setOpenId] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const anyLive = order.some((id) => jobs[id] && isLive(jobs[id].state));
  const now = useNow(anyLive, null);

  useEffect(() => {
    void useJobs.getState().load();
  }, []);

  const clear = async () => {
    setError(null);
    try {
      await useJobs.getState().clearFinished();
    } catch (e) {
      setError(toAppError(e).message);
    }
  };

  const finished = order.length - order.filter((id) => jobs[id] && isLive(jobs[id].state)).length;

  return (
    <section className="hs-view jb-view" aria-label="Jobs" data-testid="jobs-view">
      <div className="rs-head">
        <strong className="rs-title">JOBS</strong>
        <span className="spacer" />
        {openId === null && (
          <button
            type="button"
            className="icon-btn"
            data-testid="jobs-clear"
            aria-label="Clear finished"
            title="Clear finished"
            disabled={finished === 0}
            onClick={() => void clear()}
          >
            <TrashIcon />
          </button>
        )}
        {onCollapse && (
          <button
            type="button"
            className="icon-btn"
            data-testid="right-sidebar-collapse"
            aria-label="Collapse right sidebar"
            title="Collapse right sidebar (Ctrl+Alt+B)"
            onClick={onCollapse}
          >
            <ChevronIcon />
          </button>
        )}
      </div>
      {error && (
        <div className="qr-msg error" role="alert">
          {error}
        </div>
      )}
      {openId !== null && jobs[openId] ? (
        <JobDetail id={openId} onBack={() => setOpenId(null)} />
      ) : (
        <div className="hs-list">
          {order.length === 0 ? (
            <div className="qr-empty">No background jobs.</div>
          ) : (
            <ul className="jb-list" aria-label="Background jobs">
              {order.map((id) => {
                const job = jobs[id];
                return job ? <JobRow key={id} job={job} now={now} onOpen={() => setOpenId(id)} /> : null;
              })}
            </ul>
          )}
        </div>
      )}
    </section>
  );
}
