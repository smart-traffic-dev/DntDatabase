import { plural } from '../lib/plural';
import { isLive, useJobs } from '../store/jobs';
import { showJobs } from '../components/windowActions';
import './jobs.css';

/** Status bar: "n jobs" with a spinner while background jobs are queued or running; opens the Jobs panel. */
export function JobsCounter() {
  const count = useJobs((s) => {
    let n = 0;
    for (const j of Object.values(s.jobs)) if (isLive(j.state)) n++;
    return n;
  });
  if (count === 0) return null;
  const text = plural(count, 'job');
  return (
    <button
      type="button"
      className="status-jobs"
      data-testid="status-jobs"
      title={`${plural(count, 'background job')} running or queued. Show jobs`}
      onClick={showJobs}
    >
      <span className="spinner inline" aria-hidden="true" />
      {text}
    </button>
  );
}
