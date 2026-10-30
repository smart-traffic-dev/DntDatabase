import type { ReactNode } from 'react';
import { JobsPanel } from '../jobs/JobsPanel';
import { RIGHT_DEFAULT, RIGHT_MAX, RIGHT_MIN, useLayout, type RightActivity } from '../store/layout';
import { ErrorBoundary } from './ErrorBoundary';
import { HistoryPanel } from './HistoryPanel';
import { HistoryIcon, JobsIcon } from './icons';
import { Splitter } from './Splitter';
import './rightSidebar.css';

interface Activity {
  id: RightActivity;
  label: string;
  icon: ReactNode;
  hint?: string;
}

/** Only panels that exist: a disabled "coming soon" icon is noise on a bar that never goes away. */
const ACTIVITIES: Activity[] = [
  { id: 'history', label: 'History', icon: <HistoryIcon />, hint: 'Ctrl+Shift+H' },
  { id: 'jobs', label: 'Jobs', icon: <JobsIcon /> },
];

/**
 * The right edge of the window (spec §3): a panel (History or Jobs) that resizes and collapses,
 * and a 40 px activity bar that stays. Clicking the active icon collapses the panel.
 */
export function RightSidebar() {
  const visible = useLayout((s) => s.rightVisible);
  const width = useLayout((s) => s.rightWidth);
  const activity = useLayout((s) => s.rightActivity);
  const setWidth = useLayout((s) => s.setRightWidth);
  const show = useLayout((s) => s.showRightActivity);
  const setVisible = useLayout((s) => s.setRightVisible);

  return (
    <>
      {visible && (
        <>
          <Splitter
            orientation="vertical"
            reverse
            label="Resize right sidebar"
            value={width}
            min={RIGHT_MIN}
            max={RIGHT_MAX}
            defaultValue={RIGHT_DEFAULT}
            onChange={setWidth}
          />
          <aside className="rs-panel" data-testid="right-sidebar-panel" style={{ width }}>
            {activity === 'jobs' ? (
              <ErrorBoundary label="Jobs">
                <JobsPanel onCollapse={() => setVisible(false)} />
              </ErrorBoundary>
            ) : (
              <ErrorBoundary label="History">
                <HistoryPanel onCollapse={() => setVisible(false)} />
              </ErrorBoundary>
            )}
          </aside>
        </>
      )}
      <nav className="rs-activity" aria-label="Right sidebar" data-testid="right-sidebar-bar">
        {ACTIVITIES.map((a) => {
          const on = visible && a.id === activity;
          return (
            <button
              key={a.id}
              type="button"
              className={`rs-act${on ? ' on' : ''}`}
              data-testid={`right-sidebar-${a.id}`}
              aria-label={a.label}
              aria-pressed={on}
              title={a.hint ? `${a.label} (${a.hint})` : a.label}
              onClick={() => show(a.id)}
            >
              {a.icon}
            </button>
          );
        })}
      </nav>
    </>
  );
}
