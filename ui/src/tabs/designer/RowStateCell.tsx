import type { Locked } from '../../bindings';
import type { RowState } from './columnModel';

export function LockIcon() {
  return (
    <svg width="12" height="12" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true">
      <rect x="3" y="7" width="10" height="7" rx="1.5" />
      <path d="M5.5 7V5a2.5 2.5 0 0 1 5 0v2" />
    </svg>
  );
}

/**
 * The state mark of a grid row: the lock (with the server's reason and text), the row's own draft
 * errors, then modified / new / to drop. Shared by every designer grid.
 */
export function RowStateCell({ locked, lock, rowErrors, state }: { locked: boolean; lock?: Locked; rowErrors: string[]; state: RowState }) {
  return (
    <div role="cell" aria-label="State" className="dz-state">
      {locked ? (
        <span role="img" aria-label="Locked" title={lock ? `${lock.reason}\n${lock.server_text}` : 'Locked'} className="dz-lock">
          <LockIcon />
        </span>
      ) : rowErrors.length > 0 ? (
        <span role="img" className="dz-mark-error" title={rowErrors.join('\n')} aria-label={rowErrors.join(' ')}>
          !
        </span>
      ) : state === 'modified' ? (
        <span role="img" aria-label="Modified" className="dz-mark-modified" title="Modified">
          ●
        </span>
      ) : state === 'new' ? (
        <span role="img" aria-label="New" className="dz-mark-new" title="New">
          +
        </span>
      ) : state === 'drop' ? (
        <span role="img" aria-label="To drop" className="dz-mark-drop" title="To drop">
          −
        </span>
      ) : null}
    </div>
  );
}
