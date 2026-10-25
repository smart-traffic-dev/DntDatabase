import type { ReactNode } from 'react';
import { JobsPanel } from '../jobs/JobsPanel';
import { RIGHT_DEFAULT, RIGHT_MAX, RIGHT_MIN, useLayout, type RightActivity } from '../store/layout';
import { ErrorBoundary } from './ErrorBoundary';
import { HistoryPanel } from './HistoryPanel';
import { AssistantIcon, HistoryIcon, JobsIcon, ObjectInfoIcon, SnippetsIcon } from './icons';
import { Splitter } from './Splitter';
import './rightSidebar.css';

interface Activity {
  id: string;
  label: string;
  icon: ReactNode;
  /** A panel that exists yet; the others are listed so the bar shows where they will live. */
  enabled: boolean;
  hint?: string;
}

const ACTIVITIES: Activity[] = [
  { id: 'history', label: 'History', icon: <HistoryIcon />, enabled: true, hint: 'Ctrl+Shift+H' },
  { id: 'jobs', label: 'Jobs', icon: <JobsIcon />, enabled: true },
  { id: 'snippets', label: 'Snippets', icon: <SnippetsIcon />, enabled: false },
  { id: 'object-info', label: 'Object info', icon: <ObjectInfoIcon />, enabled: false },
  { id: 'assistant', label: 'AI assistant', icon: <AssistantIcon />, enabled: false },
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
          const on = a.enabled && visible && a.id === activity;
          const label = a.enabled ? a.label : `${a.label} (coming soon)`;
          return (
            <button
              key={a.id}
              type="button"
              className={`rs-act${on ? ' on' : ''}`}
              data-testid={`right-sidebar-${a.id}`}
              aria-label={label}
              aria-pressed={a.enabled ? on : undefined}
              title={a.hint ? `${a.label} (${a.hint})` : label}
              disabled={!a.enabled}
              onClick={() => show(a.id as RightActivity)}
            >
              {a.icon}
            </button>
          );
        })}
      </nav>
    </>
  );
}
